# Implemented: ephemeral-block cache seal

- **Status:** shipped `221236fc` (3728 tests green)
- **Source:** `docs/notes/proxy-experiments-2026-08.md` §§23–24


## Item 23

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 23 — `<system-reminder>` churn costs 19% of the input bill

Confirmed 2026-08-09. Item 20 inferred this from block shapes; a closed-vocabulary
classifier (`text_block_kinds`) now reads it off the wire:

```
TEXT KIND AT DIVERGENCE idx=52  [system-reminder] -> []
                                 (shapes [tool_result,text] -> [tool_result])
```

The client attaches a `<system-reminder>` text block to a `tool_result` message
on the turn it applies and removes it on the next. The provider cached the
version with the block; the next turn sends the version without, so the prefix
breaks at that message and everything after it is re-written.

The classifier reports one of `system-reminder`, `other-tag`, `plain` and never
the text — reporting the actual tag name would defeat the point, since a tag is
as user-controlled as the body it wraps.

### The leverage is brutal

95 events, 4,353,443 wasted tokens: **45,826 tokens re-written per event**, for a
reminder of a few hundred. Confirming this in the client's own transcript: user
messages holding a `tool_result` carry *only* `tool_result` blocks — 1347 of
1347. The reminder exists on the wire for exactly one turn and is never
persisted, which is precisely why it churns.

### Three ways out, none of them free

- **Hold the previous turn's version.** Keeps the cache, re-sends a reminder the
  client withdrew. They would accumulate turn over turn and never expire. Worst
  of the three.
- **Strip them entirely.** Keeps the cache, and the model never sees a reminder
  at all. They carry real instructions, so this trades tokens for behaviour.
- **Relocate them to the tail.** Strip from the historical message, re-attach on
  the newest message. The cached prefix stops churning because forwarded history
  no longer contains them, and the model still sees the reminder on the turn it
  arrives. The reminder loses adjacency to the `tool_result` it refers to.

### What was built instead: seal the cached region before the block

Relocation was approved, but reading `normalize_message_cache_control` first
turned up a fourth option that is strictly better, and it needs no content moved
at all.

That function already refuses to put the breakpoint on a proactive-expansion
block, because "its first appearance makes Anthropic write the entire segment we
were trying to preserve". A `<system-reminder>` is the same problem from the
other end. Anthropic caches up to and including the marked block, so if the
breakpoint stops short of the reminder, the reminder is never inside the cached
prefix and its disappearance next turn breaks nothing.

The model still sees it, in place, adjacent to its `tool_result`. No block is
removed, moved, or re-sent. The behaviour trade the item was weighing does not
arise.

**One correction, caught live before shipping.** The first cut sealed on any
ephemeral block anywhere in the message list. Then the classifier reported
`idx=168 [system-reminder] -> [system-reminder,plain]` — a reminder that
*persisted* across turns. A persisting reminder is part of the stable prefix, so
sealing on it would have stranded every later message outside the cache: a far
worse regression than the churn being fixed. The seal now applies only within
the final message, which is the only place a vanishing reminder can be. Pinned
by `a_reminder_deep_in_history_does_not_seal_the_rest`.

Live in `221236fc`. 3728 tests pass. Baseline to beat: 95 events averaging
45,826 wasted tokens each.


## Item 24 correction

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 24 — Item 23's 19% does not survive contact with the stream-merge confound

Same evening, after the seal shipped. A new conversation created *after* the fix
busted twice on the same reminder, which the fix was supposed to make impossible.
Its turn sequence says why:

```
17:55:32  msgs=23  declined idx=8  reminder  write=42947
17:55:33  msgs=22  full                      write= 2441
17:56:06  msgs=26  declined idx=8  reminder  write= 4221
17:56:08  msgs=26  full                      write= 2655
```

Counts running backwards and repeating, each decline followed within seconds by a
clean turn. That is item 11's fingerprint: two streams under one
`conversation_key`. The divergence at message 8 is then "stream A carries a
reminder there, stream B does not" — not "the client withdrew it".

Splitting the reminder waste by whether the key shows merged streams (only 13
events carry both `conversation_key` and the text-kind classifier, so this is a
much smaller sample than item 23's 95):

| | events | wasted |
| --- | --- | --- |
| on merged keys | 11 | 507,398 |
| on clean single keys | 2 | 339,745 |

60% of it sits on merged keys, and the clean-key share is **1.5% of the input
bill**, not 19%. Item 23's figure conflated two causes and should not be quoted.

**48 of 81 conversation keys today show merged streams.** Item 11 is marked
SETTLED because the replay *store* gained alternates; the key itself was never
changed. Every per-conversation figure in this document inherits that.

The seal from item 23 stays — on a single stream it is still correct not to cache
a block that is about to vanish, it is tested, and it cannot make things worse.
But it addresses a much smaller problem than claimed, and the larger lever is the
key.
