# Implemented: recache instruments (stream identity, no-lead, mutated, tags)

- **Status:** shipped 2026-08-22/26, some still awaiting live reads (see open
  `../recache-read-forwarded-prefix-mutated.md`)
- **Source:** `docs/notes/recache-classification.md`
- **Summary:** instead of guessing: matched-stream fields on recache events
  (settles cross-stream pairing disputes), `cache_stream_unmatched` INFO for
  match-nothing turns, `prefix_replay_no_stream_leads_turn` with
  `best_agreement_msgs` discriminator, `forwarded_prefix_mutated_after_replay`
  (+length companion) digesting replay-exit vs pre-forward bytes, `BlockTag`
  (type + canonical size) turning `2:block[0]` into `tool_result 4195B→1202B`.


## Detail

*moved from `docs/notes/recache-classification.md`*

## Open, untested

- **H5 — proxy injection into the latest user message breaks the next turn's
  prefix.** We mutate the newest message; the client resends it unmutated
  next turn. Prefix replay exists to paper over exactly this, and these
  events are all `unexplained_*after_replay*`, so if it is the cause then
  replay is not restoring what it should. Needs captured request bodies to
  test; log fields cannot settle it.
- **H6 — the second tail breakpoint writes a block nothing reads.** The proxy
  runs `--cache-tail-breakpoints 2`. A marker at the very tail creates a block
  holding the newest content, and if the next turn cannot reuse it the cost
  repeats every turn. Fits the constant ~240-token figure. Also needs
  captured bodies.

  Two data points from the offload-gap round of 2026-08-18 to 08-21, neither of
  which settles H6.

  The spare fourth breakpoint is not the answer. Swept at seven fractions from
  2% to 50% back through history, on the blindguard and windowgap corpora and
  under both weightings, every arm came out byte-identical to the untouched
  proxy. Every turn writes a cache entry at its own tail, so the conversation
  already carries a ladder of readable prefixes from its past turns and an extra
  marker lands on a rung that exists. Counting the captures directly, the client
  sends **one** message breakpoint and it is at the tail: 7,699 of 7,839
  blindguard turns and 997 of 1,009 windowgap turns. The proxy adds its own, so
  the two markers on the wire sit one block apart, which is the case
  `_tail_breakpoints`' own comment calls worthless.

  The arm that was supposed to test moving them apart never ran.
  `_spread_shipped` in `bench/strategies.py` skips any request whose message
  markers are not exactly one, which is true of the client and false of `--base
  forwarded`. So `shipped-tail-back-05` skipped every request and scored
  byte-identical to the live proxy. That read as "no effect"; it was "never ran".
  Anything else guarded on the client's marker count has the same hole.

`HEADROOM_CAPTURE_DIR` is unset on the running proxy, so no request bodies
exist for this window. Testing H5 or H6 means enabling capture and waiting
for a recurrence.


## H7

*moved from `docs/notes/recache-classification.md`*

## H7 — The classifier under-reports. OPEN, and the number is not yet trustworthy

Pairing each request with the previous request under the same
`conversation_key` finds 35 pairs with a read shortfall above the slack, all
inside the 1h TTL, together re-writing 26,051 tokens — none of them flagged.
That is nearly double the 14,207 the classifier does report. 23 of the 35
show the message count growing by one or two, which looks like an ordinary
continuing stream.

**Do not quote that number yet.** The observer does not pair turns the way
this check does. `match_stream` picks among several streams held under one
conversation key by matching message content, so a naive
previous-request-in-the-same-conversation pairing can compare across two
streams and invent a shortfall that never happened. Message counts one or two
apart do not rule that out — two streams of similar depth look identical from
outside.

Settling it needs the classifier to say which stream it matched. The recache
and ledger events carry `conversation_key` but no stream identity, so no
outside check can reproduce the pairing. **Emitting the matched stream index
on the booking event is the cheapest next instrument in this whole
investigation** — it is a few fields, and it converts H7 from an argument into
a query.


## Detail

*moved from `docs/notes/recache-classification.md`*

## Instruments added 2026-08-22 (shipped, not yet read against traffic)

Two events, no classification changes. Both exist to turn the open
hypotheses above into queries.

**`cache_recache_observed` now names the stream it compared against.** Three
fields on all four arms plus `cache_recache_ttl_expiry`:
`matched_stream_msgs` (the depth of the tracked stream the arithmetic used,
`-1` for no match), `turn_msgs` (this turn's depth), and `streams_tracked`.
This is the instrument H7 asked for. Any outside check can now reproduce the
observer's pairing instead of guessing at it, so the 26,051 figure can be
recomputed against the real pairs rather than against
previous-request-in-the-same-conversation. Until that recount runs, the
figure stays unquoted.

**`cache_stream_unmatched` (INFO) names the turn that matched nothing.** A
turn shorter than every tracked stream is filed `FirstTurn` and reports no
waste however much the provider re-wrote. For a subagent forking off a shared
opener that is correct — it had no prefix to reuse. For anything that
shortened a conversation it meant to continue it is a silent loss, and the
two are indistinguishable from inside `match_stream`. The event does not
guess; it prints `turn_msgs`, `longest_tracked`, `streams_tracked` and both
token counts so the cases can be separated after the fact.

Scale of the hole, from the unit test: a turn at depth 20 arriving after a
stream at depth 40 wrote 26,000 tokens and reported zero waste. In the
2026-08-22 window only one turn qualified and it wrote 0 tokens, so this is
real in code and unexercised in that window. Whether it is ever exercised is
now countable rather than arguable.


## Residue audit

*moved from `docs/notes/recache-classification.md`*

## Auditing the "healthy" bucket — 2026-08-23

That row never meant the provider reused the prefix; it meant the replay gate
applied one. Pairing turns **by stream** (`conversation_key`, `chain_id`) rather
than by time order, 95.2% of within-stream pairs get full reuse and 606 do not,
carrying 2,555,900 tokens. Broken down:

| bucket | turns | tokens |
|---|---|---|
| replay applied and still lost | 58 | 1,294,086 |
| TTL expiry (already named) | 4 | 527,616 |
| overlapped a still-running turn | 377 | 408,980 |
| replay declined (counted elsewhere) | 160 | 323,458 |
| idle past 1h | 2 | 1,760 |

**A request can emit both `prefix_replay_applied` and
`prefix_replay_not_replayed`.** Reading either alone misclassifies the turn —
73.4% of these shortfalls emitted both, and my first pass filed all of them as
"applied". Check for the decline first.


## Detail

*moved from `docs/notes/recache-classification.md`*

### The residue and its instrument

58 turns, 1.29M tokens, median 488 but p90 94,841 — a few very large misses
dominate, several reading 0 tokens seconds after a 1h write. No logged event
separates them from healthy turns. `ctx_inject_too_deep_for_first_sight` has a
large lift (12% vs 0.06%) but covers 7 turns and cannot account for the bulk.

The reason nothing explains them is that the deciding fact was never recorded:
whether the bytes forwarded still matched the prefix the replay believed it had
spliced in. Everything between those two points — breakpoint placement, memory
injection, context injection, PAYG rewrites — is supposed to leave the settled
prefix alone, and nothing checked that it did.

Added `forwarded_prefix_mutated_after_replay` (WARN), which digests each message
as the replay stage leaves it and again just before forwarding, and names the
first index that moved. The last two messages are excluded as this turn's live
tail. A companion `forwarded_prefix_length_changed_after_replay` catches
messages added or removed.

This is a single-request invariant, so it needs no cross-turn state and no
waiting: if it fires, a proxy stage is corrupting a cached prefix and the index
names which. If it never fires, the proxy is exonerated and the residue is
provider-side.

> **Update 2026-09-11.** The per-message check is gone — removed at
> `proxy.rs:5962-5984`. It compared within a turn, so every deterministic
> post-replay stage (tool prune, schema compaction, stable ordering, image
> optimization) tripped it: 44 fires on 44 turns proving only that those
> stages ran. The length companion survives (0 fires on 3,792 replay turns,
> 09-10) and still means what it says. Content-mutation exoneration now
> rests on the cross-turn instruments instead: `turn_cache_fingerprint`
> plus `prefix_ladder` (`tools_digest`/`system_digest` changed zero times
> over 41 consecutive pairs, so the stages are deterministic and the
> preamble is not a recache source). Do not re-add a within-turn content
> comparison — that failure mode is documented here so nobody rebuilds it.


## 2:block instrument

*moved from `docs/notes/recache-classification.md`*

## `2:block[0]` — open, and instrumented rather than guessed (2026-08-26)

Three `early_messages` recaches cost **312,861 tokens** between 2026-08-24 and
08-25 and all three read the same one-line verdict: `2:block[0]`. Slot 2's first
block was rewritten, the block count held, and nothing said which block that
was. All three prefixes had been evicted before anyone asked.

What the three have in common: deep conversations (235, 166 and 187 messages),
one event each, and an `actual_cache_read` of **21,663 on all three** — the
system and tools survive, everything after them is rewritten. The drift verdict
lands about two minutes ahead of the recache.

### What it is not

**Not the withdrawn scaffolding fixed the same day.** Replaying the withdrawal
against the 40 stored conversations that carry early scaffolding produces
`1:string,2:blocks 3->2` (20), `1:string,2:blocks 2->1` (19) and
`1:string,2:blocks 4->3` (1). A bare `2:block[0]` never appears. The
`ephemeral_spans` fix does not touch this bucket.

**Not a tool result rewritten on disk.** Across 14 transcripts that have a
`tool-results/` directory, no `tool_use_id` ever changes its stub-ness: Claude
Code substitutes `<persisted-output>` when it writes the result, never later.

**Not the interleaving that produces most `2:block[0]` lines.** 92 of them are
logged, but 86 belong to one session where the hash ping-pongs between five
fixed values (`75707f61` ↔ `ba4dff8a` ↔ `2c95db55`) and `current_message_count`
walks backwards — 4, 9, 11, 9, 13, 11, 14, 21. Those are separate request
streams sharing a session key, and they cost 31,178 tokens over 32 events. The
expensive three are `novel`: a hash that session had never held before.

Worth knowing while reading drift lines: across every rotated log, **61% of
drift verdicts (90 of 148) return to a structural hash already seen in that
session**. A conversation that mutated its history does not mutate back twice a
second. They are cheap (68,819 tokens) and the pipeline already has a name for
them — `concurrent_turn_in_flight`, 207 events, 109,127 tokens — so the drift
detector is claiming events that classifier would have taken.

### The instrument

`MessageShape` now carries a `BlockTag` per block: the block's type and the
serialized size of the canonicalized block. `early_drift` reads
`2:block[0] tool_result 4195B->1202B` instead of `2:block[0]`. Eight bytes per
block, diagnostic only, never read by the drift decision. A truncation, a
rewrite and a type substitution are three different defects and the line now
tells them apart.

### Where the money actually is

Recache cost by `attribution_reason` over every rotated log:

| events | wasted | reason / `replay_skipped` |
|---|---|---|
| 113 | 2,107,191 | `prefix_content_diverged` / same |
| 49 | 1,018,190 | `early_messages` / `no_previous_turn` |
| 536 | 527,163 | `unexplained_after_replay` / — |
| 207 | 109,127 | `concurrent_turn_in_flight` / — |

`early_messages / no_previous_turn` is the drift detector invalidating the store
and the next turn finding nothing. `prefix_content_diverged` is twice its size
and has not been read yet.
