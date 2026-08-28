# TODO: Recache Classification & Warning Behavior

**Created:** 2026-07-07 | **Updated:** 2026-07-09
**Status:** Implemented 2026-07-09 — `RecacheEventKind` (Drift/Expected) in `usage_observer.rs`, `event_kind` on `/cache-health`, two-window ⚠/ℹ statusline, Prometheus `unknown` reason relabelled `expected`. Not done: `compression_applied` plumbing (open question, not needed for the classification rule).

---

## Problem

The statusline recache warning (`⚠ recache Xm ago: ...`) fires for ALL cache invalidation events with equal severity and 10-minute persistence. Many of these are **false alarms** — not actually wasted tokens — and should not be warnings.

**Root cause:** Subagent closing or `/clear` resets conversation context, causing the usage observer to see a cache bust. But the structural hashes are identical (no drift), so the "wasted tokens" are not actually wasted — the cache was legitimately invalidated by the session ending. The cause is known and should be explained to the user, not shown as "unknown cause".

## Findings

### Log Analysis (2026-07-07, 129 total recache events)

| Category | Count | drift_dims | Cause |
|----------|-------|------------|-------|
| Drift (genuine structural change) | ~100+ | non-empty | system, tools, early_messages changed |
| Unknown (volatile content) | 20+ | empty | UUIDs, timestamps in messages[3+] |
| Compression-triggered | varies | varies | Compression modifies prefix → cache bust |
| Subagent closing | varies | empty | Conversation context reset when subagent finishes |
| `/clear` command | varies | empty | Conversation context reset by user |

**Key discovery:** 20+ "unknown" events have empty `drift_dims` — the drift detector only hashes system, tools, and first 3 messages. Root causes are **subagent closing** and **`/clear` command**: when a subagent finishes or the user runs `/clear`, the conversation context resets, causing the usage observer to see a cache bust — but the structural hashes are identical (no drift). The "wasted tokens" are not actually wasted; the cache was legitimately invalidated by the session ending. The cause is known and should be explained to the user, not shown as "unknown cause".

### Live Events (2026-07-08)

**Event 1 (~10:04 UTC) — Large waste**
```
wasted_tokens: 70,985
expected_cache_read: 90,939
actual_cache_read: 19,954
drift_dims: null
conversation_key: 32b2ea8742d2a21f
```

**Event 2 (~10:25 UTC) — Small waste**
```
wasted_tokens: 241
expected_cache_read: 58,457
actual_cache_read: 58,216
drift_dims: null
conversation_key: 2e7b2826f3f6d3a1
```

**Event 3 (~10:31 UTC) — Large waste, same conversation as Event 2**
```
wasted_tokens: 115,391
expected_cache_read: 133,816
actual_cache_read: 18,361
drift_dims: null
conversation_key: 2e7b2826f3f6d3a1
```

**Root cause:** Subagent closing. When a subagent finishes, the conversation context resets, causing the usage observer to see a cache bust — but the structural hashes are identical (no drift). The "wasted tokens" are not actually wasted; the cache was legitimately invalidated by the session ending.

**Event 4 (~10:45 UTC) — `/clear` command**
```
wasted_tokens: 97,000 (approx)
conversation_key: unknown (conversation reset)
```

**Root cause:** `/clear` command resets conversation context. Same mechanism as subagent closing — the cache is legitimately invalidated by the session ending, not by structural drift. The "unknown cause" label is misleading; the cause is known and should be explained to the user.

### Anthropic overloaded_error (2026-07-07 ~21:53-21:55)

Transient capacity errors from upstream, not proxy bugs. Request IDs: `d656d8fc`, `534fa2ab`, `2b2d0ba4`.

---

## Proposed Classification

| Condition | Level | Icon | Persistence |
|-----------|-------|------|-------------|
| `drift_dims` is not empty | Warning | ⚠ | 180s (3 min) |
| `drift_dims` is empty | Info | ℹ | 60s (1 min) |

**Rationale:**
- Non-empty `drift_dims` = genuine structural change detected (system, tools, early_messages)
- Empty `drift_dims` = subagent closing, `/clear` command, volatile content in later messages, terminal close — expected behavior, **not actually wasted tokens**
- **The cause is known** for subagent closing and `/clear` — should be explained to the user, not shown as "unknown cause"

---

## Implementation Plan (from session 2026-07-07)

### Files to modify

1. **`crates/headroom-proxy/src/cache_stabilization/usage_observer.rs`**
   - Add `RecacheEventKind` enum: `Drift` | `Expected`
   - Add `compression_applied: bool` to `PendingRequest` and `begin_request()`
   - Classify events in `complete()` based on drift_dims presence
   - Add `event_kind` field to `RecacheEvent` struct

2. **`crates/headroom-proxy/src/proxy.rs`**
   - Pass compression outcome to `begin_request()` (currently not passed)
   - Gate: drift detection runs BEFORE compression (line 1728), compression runs after

3. **`scripts/statusline-cache-health.sh`**
   - Two persistence windows: `RECACHE_DRIFT_WINDOW=180`, `RECACHE_EXPECTED_WINDOW=60`
   - Conditional icon: ⚠ for Drift, ℹ for Expected

4. **`crates/headroom-proxy/src/observability/recache.rs`**
   - Update Prometheus labels if needed (currently `reason` label: system/tools/early_messages/multi/unknown)

5. **Tests**
   - Update existing tests in `usage_observer.rs`
   - Add new test cases for Expected vs Drift classification

### Open Questions

- [ ] **Subagent hypothesis:** Do recache events with `drift_dims=null` correlate with subagent closings? Need to check logs for `subagent` or `actor` events near recache timestamps.
- [ ] **`/clear` explanation:** How should the statusline explain that `/clear` caused the cache drop? e.g. `ℹ cache cleared by /clear command`
- [ ] Does the proxy already know whether compression was applied on a given request? If so, that signal can tag the recache event. If not, need to add it.
- [ ] Should the drift detector window be extended (e.g. first 10 messages) to catch more volatile content cases?

---

## Current State (2026-07-08 ~10:45 UTC)

- **Total recache events:** 7+ (since restart)
- **Total wasted tokens:** ~392K+
- **Hit rate:** 94.2% (50 samples)
- **Last event:** ~97K tokens wasted from `/clear` command
- **Previous events:** 115K (subagent closing), 241 tokens (same conversation), 70K (different conversation)
- **TTL expiries:** 0
- **Known false alarm causes:** subagent closing, `/clear` command

---

## How to Verify the Fix

1. **Restart proxy** after implementing changes
2. **Trigger compression** (send large JSON) → should show `ℹ cache drop` (not `⚠ recache`)
3. **Wait 1 minute** → info message disappears
4. **Trigger drift** (change tools/system mid-conversation) → should show `⚠ recache`
5. **Wait 3 minutes** → warning disappears
6. **Check `/cache-health`** endpoint — `last_event.event_kind` should be `Drift` or `Expected`

## How to Reproduce the Current Bug

- **Subagent closing:** Spawn a subagent, let it finish, observe `⚠ recache` in statusline
- **`/clear` command:** Run `/clear` in Claude Code, observe `⚠ recache` in statusline
- **Volatile content:** Any conversation with UUIDs, timestamps in messages beyond index 3
- The proxy currently shows `⚠ recache` for these — should be `ℹ cache drop` (not actually wasted tokens)
- The cause is known and should be explained to the user, not shown as "unknown cause"

---

## Related Issues

- P3-35 in `REALIGNMENT/01-bug-list.md` — No cache-bust drift detector telemetry
- P3-35a (added 2026-07-08) — Drift detector blind spot: `drift_dims=null` recache events (likely false alarms from subagent closing)
- **`/clear` command false alarm:** User-triggered `/clear` resets conversation context, causing recache warning with "unknown cause" — should be info level with explanation

---

# Hypothesis ledger — 2026-08-22

The July analysis above named three causes without measuring how much each
accounts for. This section is a running ledger instead: one entry per
hypothesis, each carrying its status and the evidence that put it there. A
refuted entry stays in the file. The point is that nobody tests it twice.

**Measurement window.** Proxy PID 56134, started 13:21 local on 2026-08-22
(09:21 UTC — the log timestamps in UTC and the process start prints local,
which is worth knowing before writing any filter over it). 502 requests, 27
recache events, two conversations involved. `restart-headroom.sh` does not
roll `~/headroom-proxy.log`, so anything read from that file without a
timestamp filter mixes in older builds.

**The whole loss, this window: 14,207 tokens across 20 turns.** Small. Worth
sizing before anyone spends a week on it.

| Attribution | Events |
|---|---|
| `unexplained_after_replay` | 20 |
| `prefix_content_diverged` | 4 |
| `aftershock_of_diverged_prefix` | 2 |
| `early_messages` | 1 |

## H1 — The proxy moves the cache hot zone. REFUTED

The reason `outbound_drift_state` and `observe_outbound_drift` were built:
the inbound hash is taken before any stage runs, so proxy-caused movement in
`system`, `tools` or the first three messages could not appear in
`drift_dims`.

It is not happening. Across 502 requests the outbound detector logged 8
first-request events and 2 drift events, and both drift events paired 1:1
with an inbound drift on the same session about 60ms earlier — the client
moved, and we carried it. The `origin: "proxy"` branch has never fired.

The instrument works; the answer is no. Keep it — it is what lets the next
person skip this hypothesis in one query.

## H2 — Volatile content (UUIDs, timestamps) deep in history. REFUTED as the main cause

The July table blamed "UUIDs, timestamps in messages[3+]" for 20+ unknown
events. Measured: 3 of 27 recached turns carried any volatile finding, against
a base rate of 15/502 (3%) across all requests. Enriched roughly fourfold, so
the effect is real, but it is six findings and cannot account for twenty
events.

Locations on recached turns ran `messages[16]` to `messages[107]` — all past
the hot zone, so widening the drift hash to cover them would explain three
events and no more.

## H3 — Subagent close or `/clear` resets the conversation. REFUTED for these events

The July hypothesis. It does not fit this window. All 20 events land
mid-conversation with message counts growing monotonically (3, 15, 19, 40,
48, 59, 67, 73, 81, 91, 97, 111, 113, 121, 123) and an active prefix-replay
chain reaching `chain_id` 25. A context reset would restart that chain, not
deepen it.

It may still explain events in other windows. It does not explain these.

## H4 — Something writes a cache block that is never read. OPEN, and the strongest lead

`classify_turn` (`usage_observer.rs:385-404`) computes
`wasted = min(prev.read + prev.write − read, write)`.

In 18 of 20 events `wasted_tokens` equals a `cache_creation` figure exactly:

- **6 events** match the *previous* turn's write (243, 239, 240, 239, 209,
  248). Exact equality here means `cache_read` came back precisely equal to
  the previous turn's read — the block written last turn was not read this
  turn, at all. That is a block paid for and never used.
- **12 events** equal *this* turn's write, which is the clamp in the formula
  binding, so they only tell us the shortfall was at least that large.

The six exact matches are the sharp signal, and the repeated ~240-token
figure suggests one fixed block rather than a drifting prefix.

## Measured and uniform, so not discriminating

Prefix replay's stable window ends exactly one message short of the end
(`total − stable == 1`) on all 20 events. That is by design — the newest
message is new — and it holds on clean turns too, so it separates nothing on
its own. Recorded here so it is not mistaken for a finding.

Unexplained turns are *shorter* conversations than average (median 67
messages against 308) with higher output (691 tokens against 273). Neither
has an explanation yet.

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

`HEADROOM_CAPTURE_DIR` is unset on the running proxy, so no request bodies
exist for this window. Testing H5 or H6 means enabling capture and waiting
for a recurrence.

## Is the classifier calling things waste that are not? Mostly the opposite

Worth stating the bias deliberately, because it decides which way to fix
anything found here: flagging a turn that turns out to be benign costs a
look, while filing a fixable loss as expected hides it for good. Prefer the
first. Every entry below was checked in that direction.

**The TTL path was already fixed and is correct now.** `classify_turn` files
a bust as `TtlExpiry` when the gap exceeds the TTL it is told about. Left at
the 5-minute default while the proxy pins 1h, every bust in the 5m–1h gap
reads as a legitimate expiry — `proxy.rs:580-582` records that this hid about
3% of daily creation. `with_cache_ttl(observed_cache_ttl)` now passes the
pinned value, and the running proxy has `--force-1h-cache-ttl true`.

**The slack early return hides nothing here. REFUTED.** `classify_turn`
returns `Healthy` when the re-write is under `RECACHE_SLACK_TOKENS` (64),
on the reasoning that nothing meaningful was billed. That could in principle
swallow a small loss repeated every turn. Measured across this window: **0
turn-pairs** hit it with a read shortfall above the slack. Not a leak.

**The clamp is right, not conservative.** `wasted = min(shortfall, write)`
caps reported waste at what was actually paid to re-create. Without it a
conversation that simply got shorter would report the entire missing read as
waste. Twelve of the twenty events sit on this clamp, which means their true
shortfall was larger — but the extra was never billed, so it is not waste.

**Turns escaping accounting. REFUTED.** If a turn never reached `complete()`
its loss would be invisible. Measured: 577 requests forwarded, 577 booked,
2 unbooked. Booking is not the leak.

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

## Where the tokens actually go — 2026-08-23

Measured over 82MB of logs, 2026-08-20 to 08-22: 14,290 turns, 45.9M tokens of
cache creation. Attribution by cause:

| cause | turns | creation | share |
|---|---|---|---|
| replay applied (healthy) | 13,604 | 31.7M | 68.9% |
| cold start, no stored prefix — legitimate | 363 | 11.0M | 23.9% |
| **no held stream leads the turn** | **51** | **2.59M** | **5.6%** |
| declined, real chain, content diverged | 271 | 0.52M | 1.1% |
| shorter than stored prefix | 7 | 0.15M | 0.3% |

91.4% of consecutive turn-pairs get full reuse. The system is mostly working,
and the loss is concentrated: 51 turns carry 2.59M tokens, ~50k each. That is
the one number worth chasing.

### Refuted, with the numbers that killed each

**H-marker: the tail breakpoint moving orphans the previous block.** Both
markers do slide forward every turn and no path re-declares an old position
(`place_tail_cache_breakpoints`, `prefix_replay.rs:1726`). But `cache_control`
is in `NON_SEMANTIC_KEYS` (`prefix_replay.rs:391`) and is stripped before any
prefix comparison, and the provider matches on content, not marker parity.
Dead.

**H-slack: `TAIL_EDIT_SLACK = 2` is off by one.** The tail-edit rescue needs
the divergence within 2 messages of the stored prefix's end. Gap distribution
across 288 divergences: 219 at gap 1, 56 at gap 2, **4 at gap 3**, worth 8,476
tokens. Raising the slack recovers nothing. Dead.

**H-race: concurrent turns lose each other's writes.** Real, but not a defect
and not ours. Of 143 pairs where the previous turn's write was never read, 106
were requests that started before the previous one finished — 53.7% overlap
against a 0.8% base rate, a 67x enrichment. Those are parallel subagents under
one `conversation_key`; pairing them by time order is invalid, so most of that
apparent 304k loss is an artifact of the offline pairing, not a real loss.
**Any log query that pairs turns by conversation key and time order is wrong
wherever subagents run.** Pair by stream.

**H-retry: the losses follow dropped streams.** Of 12 equal-length in-place
divergences, 0 had any retry, overload, error or stream-drop event on the
previous turn. Dead.

### What the divergences actually are

`prefix_replay_not_replayed` already carries `diff_shape_stored` /
`diff_shape_current`, which went unread until now. Across 319 diverged turns:

- 116 — blocks **appended** to an existing message, mostly
  `tool_result` -> `tool_result,text`. The client attaching a
  `<system-reminder>` to a message it already sent. Client behaviour.
- 76 — same shape, content edited inside.
- 59 — stored says `string`, current says `thinking,tool_use`: **different
  speakers at the same index**, so the comparison was against another stream
  entirely. 44 of these are `chain_id == 0`.
- 8 — blocks removed.

60.7% of all divergences sit at exactly `len-2`, and 100% of the equal-length
ones do. `len-2` is the assistant message; `len-1` is the user tool_result.

### The open question, and the instrument for it

`chain_id == 0` from `previous_turn_for` is deliberate and correct: it means no
held stream leads this turn, so the store refuses to splice rather than merge
two unrelated runs (`prefix_replay.rs:2544-2552`). The question is why a
*continuing* stream finds nothing held. Three causes need opposite answers — a
genuinely new stream (nothing to do), an entry evicted under
`MAX_ALTERNATE_PREFIXES` (16) or `MAX_ALTERNATE_MESSAGES` (4,000), or a real
divergence after a long agreement.

Eviction is counted only in `proxy_cache_replay_alternates_evicted_total`,
which is lazily registered and absent from `/metrics` on a fresh process, so it
could not answer this retroactively.

Added `prefix_replay_no_stream_leads_turn` on that arm: `alternates_held`,
`held_messages`, `primary_prefix_msgs`, `current_msgs`, `best_agreement_msgs`
and both caps. `best_agreement_msgs` is the discriminator — 0 means eviction or
a brand-new stream, a long run means identity was nearly there. This needs the
new binary running; the 08-22 restart predates it.

## Root cause found — 2026-08-23

**`MAX_ALTERNATE_PREFIXES = 16` was the only bound that ever bit, and it threw
away the most expensive prefix in the store.**

Reproduced without traffic, in a unit test. A 300-message conversation, then
subagents at 30 messages each:

```
main=300 sub= 30: LOST after 17 subagent turns (held  480/4000, cap 16)
main=300 sub=100: LOST after 17 subagent turns (held 1600/4000, cap 16)
main=300 sub=250: LOST after 16 subagent turns (held 3750/4000, cap 16)
```

The parent is dropped after 17 subagent turns while the store holds 480
messages against a 4,000 budget — 12% of the bound the code calls "the bound
that actually matters". The count ceiling bit first in every shape tested.

Two things combine. Eviction takes the least-recently-*displaced* entry, on the
reasoning that it "has actually gone quiet"; a parent waiting on its fan-out
looks exactly like that. And the parent is the largest entry in the store, so
the cheapest thing to keep by count is the most expensive thing to lose by
tokens. Its next turn then finds no stream leading it, takes the `chain_id == 0`
path, and re-caches everything — the 51 turns and 2.59M tokens above, ~50k
each.

**Fix: raise the ceiling to 128** so the message budget governs, which is what
the design intended. Memory is unchanged: it is bounded by
`MAX_ALTERNATE_MESSAGES`, not by the count.

The number is sized against the data, not picked round. Distinct streams per
session across the same logs — `chain_id` increments once per new stream, so
its max per session measures exactly this:

| | streams |
|---|---|
| median | 0 |
| p90 / p95 / p99 | 4 / 6 / 11 |
| max observed | 29 |
| sessions over 16 | 2 of 344 (0.58%) |
| sessions over 32 | 0 |

128 is 4.4x the busiest session ever seen. Note that `alternates_held` in the
logs maxes at exactly 16 — the old ceiling clipping its own distribution, which
is why the count had to be measured through `chain_id` instead.

Subagent *turns* are not the quantity that matters: a stream taking many turns
is promoted back to primary on each one and consumes no extra slot. Only
distinct streams do.

Pinned by `a_parent_conversation_outlives_a_large_fan_out`. It fails at 16 and
passes at 64, so the defect cannot come back quietly.

### Eviction order — fixed too

The raised ceiling does nothing for a session that genuinely exhausts the
4,000-message budget; recency ordering still dropped the parent first. Recency
turned out to be the wrong signal outright. An entry's position records how
many *other* streams have taken a turn since, so a parent blocked behind a
fan-out ages exactly as fast as one that has finished.

Size is the better predictor, and the logs say so plainly. Grouping turns into
streams by `(conversation_key, chain_id)`, the chance a stream ever takes
another turn against how much it has cached:

| stream size (cached tokens) | n | median turns | P(>=2 turns) | P(>=10) |
|---|---|---|---|---|
| 0-10k | 30 | 1 | 0% | 0% |
| 10-30k | 68 | 1 | 16% | 1% |
| 30-60k | 224 | 2 | 51% | 8% |
| 60-120k | 483 | 5 | 76% | 28% |
| 120k+ | 275 | 13 | 92% | 57% |

Monotonic across every bucket, log-log r = 0.54 over 1,080 streams. Because the
budget is counted in messages and tokens track messages, value per unit of
budget is exactly that probability, so the budget belongs to the large streams
— and dropping a small one costs close to nothing, since it was never coming
back.

Eviction now selects by size descending with recency as the tiebreak, and skips
rather than stops, so a small stream can still use the room a rejected large one
left. Pinned by `the_budget_goes_to_the_stream_most_likely_to_return`, which
fails under recency-only ordering.

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

### Refuted here

- **TTL.** 217 of 218 sequential shortfalls followed a 1h write, with gaps in
  seconds. Not expiry.
- **The proxy rewriting the front of history.** `messages_rewritten` touches
  index <=3 on 95.4% of shortfalls and 95.2% of healthy turns — identical, so
  it discriminates nothing. Rewriting the front is universal and normally
  harmless.
- **Context offload volume.** Lower on the residue (7 blocks) than on healthy
  turns (12), the wrong direction for a cause.

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

## Older threads, closed — 2026-08-23

**Concurrent turns now have a name.** 72% of overlapping turn-pairs lose cache
against a ~5% baseline (377 pairs, 408,980 tokens), and 374 of them had a replay
applied — the splice was right, the timing was not: the provider had not
committed the previous turn's write because that turn was still streaming
(median 3.4s left). These were landing in `unexplained_after_replay`.

`recache_attribution` now returns `concurrent_turn_in_flight` / origin `client`.
Read from the observer's own pending map, not from timestamps — this machine's
wall clock steps backwards under load. **Still counted as waste**: the tokens
were genuinely re-billed, and filing 409k tokens as expected would retire them
into a bucket nobody reads. Checked after every structural cause, so a real edit
still wins. Pinned by `a_turn_racing_its_own_conversation_is_named_but_still_billed`
and `a_named_cause_outranks_concurrency`.

**Both crush flags are dead.** `--min-tokens-to-crush` (`config.rs:1326`,
default 200) and `--max-items-after-crush` (`config.rs:1334`, default 15) are
declared, copied into the runtime `Config`, and never read by the request path.
The live `SmartCrusher` is built once from `SmartCrusherConfig::default()` at
`live_zone.rs:607`, so the CLI values cannot reach it. Same three-layer pattern
as the previously documented dead flag. Changing either from the command line
has no effect on forwarded requests.

**The 3.1% vs 0.16% rebuild-boundary gap is a denominator mismatch, not a
disagreement.** Both come from the same condition — `observe_drift(...).is_some()`
(`drift_detector.rs:467`), used identically by the replay path
(`proxy.rs:2797`) and the J4 offload gate (`proxy.rs:3086`). There is no second
definition. 0.16% (4 of 2,571) counts only turns that emitted a `ctx_offload`
line, which fires solely when a turn had an offload candidate
(`proxy.rs:3113`). 3.1% (245 of 7,839) counts every turn in the replay corpus
(`offload_replay.rs:213-235`). Narrow subset versus whole corpus. Neither
number is wrong; quoting them side by side is.

**`SessionReplayStore::invalidate` has no production caller.** It is an
ordinary `pub fn` (`prefix_replay.rs:2692`), and the module doc
(`prefix_replay.rs:68-71`) says it "is called on a rebuild boundary to drop the
stored prefix" so a stale prefix cannot be replayed after the provider's cache
died. Every call site is in tests (`prefix_replay.rs:3558, 3763, 4501`). The
documented behaviour does not exist: after a hot-zone change the store keeps its
prefix and the chain id carries across the boundary. This is a live candidate
for part of the 58-turn residue above — worth wiring or worth deleting from the
doc, but not worth leaving as a claim that is not true.

# Two causes found and fixed — 2026-08-24

Measured on a 1,622-request capture (`~/headroom-capture-alpha`) and on the
proxy log since the 20:30 restart. Method: hash every message of every turn
with `cache_control` stripped, then compare each turn against the one before it
in the same session. `cache_control` has to go, because the tail breakpoint
moves every turn by design and swamps everything else.

| | pairs |
|---|---|
| clean append | 1,593 |
| tail edit, 2 messages deep or less | 114 |
| **deep divergence** | **3** |

Three. And those three are the whole `prefix_content_diverged` class:
143,630 tokens, the largest remaining waste on the current build.

## The working-directory pin ran after the hash that judged it

`hold` fires correctly — the log carries `working_directory_held` on both
worktree turns. But `compute_structural_hash` runs about 1,100 lines earlier,
on the client's body, and `replay_store.invalidate` (`proxy.rs:2847`) threw the
stored prefix away 17ms before the pin restored the very line it tripped on.
The turn then forwarded with `replay_skipped: no_previous_turn` and re-cached
46,707 tokens.

The rule was already written down, above the billing-header pin
(`proxy.rs:2696`): "This has to run here, ahead of the fingerprint below and
the prefix-replay capture further down, so every stage sees the pinned form."
The working-directory hold was the one stage breaking it.

Fixed with `WorkingDirPins::preview`: rewrite `system` to the held directory,
hash that, put the client's `system` straight back. It shares `hold`'s decline
conditions — no pin, expired pin, changed line count — so the two can never
disagree about what will be forwarded.

**Entering a git worktree is not covered, and should not be.** Claude Code adds
two lines alongside the path ("This is a git worktree…", "The git stash stack
is shared…"). Pinning the path alone still leaves those changed, so `preview`
declines and the rebuild is correct. Holding them would tell the model it is
not in a worktree while it is.

## A `SubagentStart` hook deleted a message 48 positions deep

All three deep divergences are the same `role: "system"` message:

```
prior[151] user      <teammate-message teammate_id="team-lead" …>
prior[152] system    SubagentStart hook additional context: Code discovery…
…
cur[199]   user      <teammate-message teammate_id="team-lead" …>
cur[200]   system    SubagentStart hook additional context: Code discovery…
```

A long-lived teammate agent, woken again by `SendMessage`. The hook fires on
every wake and appends its reminder at the tail; Claude Code deletes the older
copy rather than duplicate it. That deletion shifts every message after it, and
one of the three cost 132,539 tokens.

The text was identical on every firing — no per-invocation content at all — so
a hook bought nothing over static context. Removed the `cbm-subagent-reminder`
entry from `acme-api/.claude/settings.local.json` and moved the two sentences to
that project's `CLAUDE.md`.

`CLAUDE.md` does not reach the `system` block, as it happens — it arrives as a
`<system-reminder>` inside `messages[0]`, the front of the array, which is the
most stable position there is. Verified in the capture: all three files
(`~/.claude`, `~/workspace`, `acme-api`) appear there, in 961 of 961 teammate
conversations and 184 of 184 main ones under acme-api. So subagents do get it,
once, at 0.1x forever.

Scoping: `codebase-memory-mcp` runs only in acme-api, so the guidance stays in
that project's `CLAUDE.md` and not in the user-level one. One wiring existed
(the project's `settings.local.json`), which every config dir reads, so
`.claude`, `.claude-personal` and `.claude-work` are all covered by the single
edit. The script itself survives, unwired, at `hooks/cbm-subagent-reminder`
under `.claude-personal` and `.claude-work`; its header says
"Installed by codebase-memory-mcp", so check the wiring again after that
server next installs.

## Correction to the entry above

"`SessionReplayStore::invalidate` has no production caller" is no longer true.
It is called at `proxy.rs:2847`, on the rebuild boundary, exactly as the module
doc describes — and calling it a beat too early is what caused the first bug on
this page.

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

## `prefix_content_diverged` — 95% of it was one predicate (2026-08-26)

The 2026-08-24 entry above closed this class at three events and two causes.
It reopened. Scoped by process start, the waste never went away:

| window | started | events | wasted |
|---|---|---|---|
| 18 | 2026-08-23 20:54 | 12 | 665,481 |
| 19 | 2026-08-24 12:02 | 8 | 200,538 |
| 21 | 2026-08-25 08:07 | 34 | 357,669 |
| 22 | 2026-08-26 06:54 | 19 | 467,348 |

Window 22 is the process running now, on the current build.

### One signature, and it is exact

Of 102 content-divergence declines since 2026-08-25, 88 splice and cost little.
The 14 that recover nothing are the same 14 three times over:

- `replayed_prefix_msgs == 0` ⟺ `chain_id == 0` — no held chain, so
  `replay_upto` is 0 and the whole stored prefix goes
- 12 of the 14 report `first_diff_path: role`, `diff_shape_stored: string`,
  stored head `system`, current head `assistant`

That is a withdrawn `role: "system"` message: every message behind it shifts by
one and the index-aligned compare meets an assistant where it stored a system.
**1,998,513 tokens over 33 turns, 95% of all `prefix_content_diverged` waste on
record** — 98% of it since 2026-08-25.

### Why the existing guard missed them

`align_over_withdrawn_scaffolding` was already there and already correct in
shape. It asked `is_pure_client_scaffolding`, which keys on the
`<system-reminder>` wrapper. The wrapper is optional.

Of the 3,050 `role: "system"` messages across the 114 stored prefixes, 593 carry
the tag and **81% do not**. The bare ones are output-style banners, `PreToolUse`
hook context, skill and agent listings and `Note:` file notices. Claude Code
sends the **same** `PreToolUse:Bash` text both ways — 468 tagged, 656 bare — so
the tag cannot be the test.

Counted 2026-08-26 against a store the proxy is still writing to, so the totals
drift between readings; the ratio holds.

`role` can. The Messages API carries the system prompt in a top-level field, so
a `role: "system"` entry inside `messages` never comes from the user or the
model. Index 0 is excluded: an OpenAI-Chat body puts its real system prompt
there, and losing that is a changed prompt. The proxy's own converters
(`handlers/gemini.rs`, `handlers/batch.rs`, `handlers/local_model.rs`) all push
theirs first, and none of the 114 stored conversations opens with one.

### Measured

`is_client_scaffolding_message` replaces the tag test in the replay comparator
and in the drift detector's early window. Replaying the withdrawal against every
persisted conversation:

```
conversations holding scaffolding: 101
  prefix survives the withdrawal BEFORE: 64
  prefix survives the withdrawal AFTER : 101
```

Reproduce with `price_the_role_predicate_against_persisted_conversations` in
`tests/early_reminder_drift_proof.rs`.

The cost of the blindness is unchanged and already documented above: replay
forwards the stored copy, so a withdrawn banner stays on the wire inside the
cached prefix at 0.1x. Watch `outbound_body_bytes` against
`client_request_bytes`; a ratio past ~1.2 means the accumulation is real.

## Accumulation watch — read, and clear

That threshold sat above with no reading behind it for two days. Taken
2026-08-26 over 29,321 priced turns:

```
day          turns   median     p90     max   over 1.2
2026-08-20      42    0.895   0.924   1.002      0
2026-08-22    3575    0.964   0.983   1.202      1
2026-08-23    6987    0.955   0.996   1.112      0
2026-08-24    6646    0.965   0.988   1.172      0
2026-08-25    6440    0.967   0.989   1.213      1
2026-08-26    5631    0.968   0.984   1.056      0
```

Forwarded bodies run 3-4% **smaller** than what the client sent, flat across six
days, and two turns out of 29,321 crossed 1.2. Nothing accumulates.

Read it again once the `role` predicate has been live a few days: it widens what
replay forwards from the stored copy, so it is exactly the change that could
move this number.

## `unexplained_after_replay` — closed, nothing to chase

536 events, 527,163 tokens. The largest bucket nobody had opened, and it is not
a re-cache at all.

Every event carries `matched_stream_msgs == turn_msgs == prefix_stable_msgs`:
the stored prefix matched the turn end to end, the replay went out, and the
provider read back a little less than the ledger expected. The distribution says
the same thing twice:

```
median   690        p90 2,034        p99 6,223        max 9,161
   0-  200:  12 events      2,014 tokens   0.4%
 200- 1000: 344 events    170,793 tokens  32.4%
1000- 5000: 173 events    306,104 tokens  58.1%
5000-20000:   7 events     48,252 tokens   9.2%
    20000+:   0 events
```

No tail. `prefix_content_diverged` put 1,998,513 tokens into 33 turns; the worst
single turn here is 9,161, and the total is a flat ~700 spread over 536 turns.
That is a breakpoint landing a block short of the divergence, or a 5m block
ageing out under a 1h one — the granularity of the provider's own accounting,
not a prefix we broke.

Leave it. Re-open if the max reaches five figures, which would mean something
real had started hiding behind the name.

## `concurrent_turn_in_flight` — the name is honest

208 events, 150,652 tokens, median 241. Cheap enough to ignore, but it had been
taken on trust: the flag is set in `begin_request` from any other *pending*
entry under the same conversation key, and `pending` is a 512-slot LRU with no
timeout. A turn that never calls `complete` — client disconnect, upstream error
— leaves an entry behind that would mark every later turn on that key as
concurrent until the LRU pushed it out. That failure mode would be invisible in
the counts and would quietly absorb waste with some other cause.

It is not happening. Timing every flagged event against the nearest other turn
on its own key:

```
                          flagged (208)      other reasons (752)
sibling within  2s          44.7%                 34.0%
sibling within 10s          93.3%                 81.2%
sibling within 60s         100.0%                 94.6%
sibling beyond 60s             0                    40 events
```

Not one flagged event has its nearest sibling more than a minute away, where the
control bucket has a 5% tail out to ten minutes. A stale pending entry would
show up precisely as a flagged event standing alone in time, and there are none.

The attribution also still predicts what it claims to. Over all 29,432 turns,
splitting on whether another turn on the same key completed within 2s:

```
overlapping    2,034 turns    349 re-cached   17.16%
alone         27,398 turns    611 re-cached    2.23%
```

Overlap raises the re-cache rate 7.7x. The mechanism in the code comment is
real, the label points at it, and the bucket is small. Nothing to do.

## Pre-restart baseline (2026-08-26)

Captured before the restart that puts the scaffolding predicate, the `BlockTag`
instrument and the transport `cause` fields into the running process. Nothing
below is live yet, so this is the "before" column and the only one that will
ever be measurable.

```
reason                                    all-time            2026-08-26
prefix_content_diverged        116 ev    2,108,921     20 ev      468,393
early_messages                  53 ev    1,129,413      3 ev      317,384
unexplained_after_replay       537 ev      527,269     71 ev       60,962
system                           9 ev      183,268      1 ev       34,549
tools                            5 ev      166,873      1 ev       67,512
concurrent_turn_in_flight      208 ev      150,652     44 ev       55,816
shorter_than_stored_prefix       3 ev       31,460      0 ev            0
system,early_messages           11 ev       12,479      0 ev            0
aftershock_of_diverged_prefix   15 ev        6,915      0 ev            0
TOTAL                          957 ev    4,317,250    140 ev    1,004,616
```

Read the all-time column with care: it spans several binaries. `86bd3fc2` added
the `origin` field partway through, so 42 events and 401,156 tokens before
08-24 carry a reason with no origin and cannot be split into client-caused and
proxy-caused. The 08-26 column is one day and one binary, and is the honest
comparison point.

Two predictions worth holding the authors to, both from the scaffolding
predicate:

- `prefix_content_diverged` should mostly go. 1,998,513 of its 2,108,921 tokens
  are the withdrawal the predicate now steps over.
- `early_messages` should fall a long way too, and this has *not* been priced.
  Its worst turn, 145,891 tokens on 08-26T08:28:32, is the same withdrawal seen
  from the drift detector's side, and the early-window filter now skips
  scaffolding. The top ten events hold ~930k of the 1,129k; the median is 1,349.
  If the number does not move, the filter is not reaching this path and that is
  the next thing to find out.

### Why the process now logs its own identity

Scoping a measurement "by process start" is the rule every number on this page
depends on, and it could not actually be followed. The log is appended across
restarts and reboots — five files, 22 runs — and carried exactly one
`headroom-proxy starting` marker in the current file, at 06:54:34Z, while the
process that wrote most of that file began at 07:26:23Z. Boot time checks out
(`/proc/stat` btime agrees with `/proc/uptime` to 0.09s), so this is not clock
drift; a run genuinely reached the log without announcing itself, and why is
still open.

`main.rs` now puts `pid`, `version`, `binary_len` and `binary_mtime` on the
starting line. Version alone cannot separate two builds of `0.1.0`; size and
mtime can. After the restart, "which binary produced this event" is answerable
from the log instead of from memory — which is how the counts on this page went
stale once already.
