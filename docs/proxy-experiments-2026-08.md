# Proxy experiments and findings (2026-08)

**Closed.** Every item below was resolved between 2026-08-09 and 2026-08-12.
The closure evidence — what was measured, and what changed — is in
[proxy-experiments-closures.md](proxy-experiments-closures.md). Nothing here is outstanding work.

It started as notes from watching one live proxy run, and it is kept now for
what it refutes rather than what it proposes. Item 19 is a fix that was built,
measured and reverted. Item 25 lists three premises that each looked
well-supported when acted on, and the cheap check that would have killed each
one. Item 27 is the rule that `prefix_replay_applied` means the forwarded bytes
changed, not that the replay worked — it invalidated item 21's headline. Read
the relevant item before rebuilding any of this.

Every figure is scoped to the window its own item names, and some are
superseded: item 3's totals are marked do-not-quote after item 11's fix. Item
numbering follows the order things were found, not the order they were worked,
and there is no item 17.

**Run observed:** pid 15975, started 14:33:08 +04 (10:33:08Z), binary
`~/.local/bin/headroom-proxy`, log `~/headroom-proxy.log` (unrotated — scope
every query by process start, see memory note).

**Window analysed:** 10:33:08Z–11:40:27Z, then live tail past 11:46Z.
5844 log lines, 340 forwarded requests, 311 booked turns.

Already covered elsewhere, not repeated here: empty `drift_dims` recache
events are classified as expected (subagent close, `/clear`) in
[TODO-recache-classification.md](TODO-recache-classification.md). The 534,485
"expected" tokens below fall in that bucket and are excluded from waste totals.

---

## Read this first

The items are numbered in the order they were found, not the order they should
be worked. They are also entangled: two of them can invalidate others, so
working top-down will waste effort.

**Settle these two before touching anything else.**

| | why it comes first |
| --- | --- |
| ~~**11** — two streams, one `conversation_key`~~ | **SETTLED 2026-08-09 and fixed.** The key was too coarse: it merged interleaved streams, so 3, 3a and 3c are partly an accounting artefact and their totals shrink. Details below. Anything quoting item 3's waste needs re-deriving. |
| **1c/1d** — `tok_saved` is cumulative | The same defect produces 1, 1a, 1b and the run-total in 2. Fixing it once closes four items. Root cause located at `proxy.rs:3144-3147`: the baseline is measured after the CTX transforms, and their saving is then added back in. |

**Then, independent of the above:**

- **10** — ledger prices saved tokens at fresh-input rates. Self-contained,
  needs a decision on the right rate rather than an investigation.
- ~~**9**~~ — **ANSWERED 2026-08-09, see item 18.** The requests with no
  completion record are the ones Anthropic rejected with 400. A rejection is not
  an event stream, so the SSE parser never runs and nothing downstream of it
  fires. 9a's suspect — a dropped `JoinHandle` swallowing a panic — is not
  needed to explain it. The 4 `stream_incomplete` requests remain a separate,
  benign failure, as 9b said.
- **12** — nine code paths with no instrumentation. Not defects; the reason
  several items here took so long to find.
- **14** — a rate limit is followed by cold caches: 3 cold starts in the two
  minutes after the 22:16Z burst, 326K tokens of prefix rewrite, on a gap
  shorter than the TTL. Ties into 7's clamp decision.

**Needs a decision, not a fix.** These are written up as defects but are
really open questions:

- ~~**4**~~ — **decided by measurement 2026-08-09, fix landed.** With
  `conversation_key` now on the event, the question the item could not answer
  became a query: of 37 warning sites seen in two or more requests on one
  conversation, **none ever changed value**. All 4,004 warnings in the window
  came from values holding still. Option (a) is implemented — a location is
  reported once per conversation and then only when its value moves. Roughly
  37 warnings where there were 4,004.
- **8** — three optimisations are inert under non-PAYG auth. Correct
  behaviour, but worth confirming nobody attributes savings to them.

**One item was wrong.** Item 7 claimed the proxy ignores `Retry-After`. It
doesn't — `proxy.rs:3543-3564` reads and honours it. The item is rewritten as a
missing log field and is now a one-line fix. The lesson generalises to this
whole document: **an absent log field was read as absent behaviour.** Items
resting mainly on "the log never says X" — 4's caveat, 5, and the 9/9a split —
deserve a source check before anyone acts on them.

**Confidence.** Three tiers:

- *Measured and reproducible* via `scripts/proxy_log_audit.py`: 1c, 4, 9, 10,
  11, 13.
- *Confirmed in source*: 1d, 5, 6, 7, 9a's discarded handle, 11's key
  derivation. File:line given in each item; all read directly, not inferred.
- *Hypothesis*: 9a's panic explanation, and 1d's attribution of the excess to
  the CTX transforms. Both name the check that would settle them. Neither is
  proven — do not fix on their strength alone.

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

### Item 11a — explained, not a collision

The "pinned value crossing two distinct keys" has a duller cause than a hash
collision. All four early events carried the same `prefix_head`
(`553665a182ec00dc`) — the same model, system and tools block — so
`expected_cache_read` measured the same cached block in each, landing at 41,023
and 41,020 on different conversations. Two conversations sharing a system
prompt and a tool list will agree on that number without sharing anything else.
A second head (`ba02a1f22f73`) shows up on the other session, so the field does
discriminate.

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

### 8 — the three inert passes are a deliberate risk trade, and there is a flag

Checked in source 2026-08-09. E1 (tool-array sort), E2 (schema-key sort) and E3
(`cache_control` auto-placement) skip on non-PAYG auth, and a Claude Code
session classifies as `Subscription` by User-Agent even though it carries an
OAuth-shaped token (`auth_mode.rs`).

The gate is **not** a claim that they would not help. Every stated reason is
about how byte mutation *looks* to the upstream — "reordering bytes for a
subscription client can look like cache-evasion and trigger revocation".

Item 8's actual question — is anyone crediting savings to passes that never ran?
— is **no**. The skip path never appends to `strategies_applied`, and even when
these passes do run they report `tokens_before: 0, tokens_after: 0`; they
contribute nothing to `tokens_saved` in either case.

`--auth-mode-policy-enforcement disabled` forces the PAYG pipeline and unlocks
all three (commit `7348ede3`, whose message argues token reduction matters for
subscription users too). It defaults to `enabled`, so they stay off. **The flag
is coarse** — it also changes compression policy, internal-header stripping and
synthetic-header injection — so it trades an account-safety posture for token
savings on every request. That is an owner's decision, not a defect to fix.

### Settled by the telemetry (2026-08-09, new binary live at 04:02Z)

- **16 — FIXED, root cause found.** Reproduced offline in
  `tests/integration_digit_integrity.rs`, then fixed. It is
  `search_compressor`, and the mechanism is not a time parser at all:
  `parse_match_line` reads a grep hit as `<path><sep><digits><sep><body>`, and
  an ISO-8601 timestamp fits that shape exactly —
  `2026-08-08T23:02:36.174635Z` parses as path `2026-08-08T23`, line `2`, body
  `36.174635Z`. The renderers then rebuilt the line with
  `format!("{}:{}:{}", file, m.line_number, m.content)`, and because
  `line_number` is a `u64` the minute came back unpadded.

  Every detail of the live observation follows from that and confirms it: only
  the *minute* is damaged (it is the line-number slot), the date survives (it is
  inside the path slot), seconds and microseconds survive (inside the body), the
  hex key survives (no colons), and `22:32:11` survives *because 32 has no
  leading zero to lose*. That last one is the clincher — a generic
  leading-zero stripper could not spare it.

  Fix: `SearchMatch` now carries the source line and the renderers emit it
  verbatim. This compressor selects lines; it does not get to rewrite them.
  Rejecting the parse instead was considered and dropped — unparsed lines are
  *dropped* (`lines_unparsed`), so that would trade corruption for data loss.
  A mis-parse can still group a line oddly, which is a ranking bug; it can no
  longer change a digit, which was a data bug.

- **1 / 1a / 1b / 1e — not a defect, closing.** 36 consecutive turns on the new
  binary: `tok_before=64912 tok_saved=56369 tok_after=8543`, unchanged while the
  conversation grew by 76 messages, and **zero negative `tok_after`**. The
  pinning is real and correct. `outbound_body_bytes` proves it:

  ```
  in=1,384,317  out=1,181,400  delta=202,917
  in=1,395,911  out=1,192,994  delta=202,917
  in=1,407,598  out=1,204,731  delta=202,867
  ```

  `bytes_in` and `bytes_out` both climb with the conversation; only the delta
  holds. The client re-sends the same large tool results every turn, so the
  compressor compresses the same blocks every turn and frees the same 56,369
  tokens every turn. Each request really is ~203 KB smaller. A stateless
  transform re-doing identical work on re-sent history *should* report an
  identical per-request saving — "re-emission" described correct behaviour.

  What was actually broken was the subtraction, and that was 1d. What is left is
  not a bug: summing a correct per-request saving across 36 turns yields 2.03M
  "tokens saved" for one compression re-applied. That is item 2 and item 10's
  semantics question, and it needs a decision about what the headline number
  means — not a code fix.

- **9 — not reproducing on the new binary.** 35 dispatched, 35 `sse stream
  closed`, 35 booked, **0** task failures, **0** missed chunks, no gap. The
  12–16% blind spot did not appear in this window. That is not proof it is gone
  — the original was bursty (33% in one hour, 4% in another) and 35 turns is a
  small sample — but there is no live failure to diagnose, so there is nothing
  to fix yet. The instrumentation is armed and will name the cause when it
  recurs.

### Fixes started (2026-08-09)

- **1d:** `proxy.rs` books only the compression dispatcher's own per-turn figure,
  so `tok_after` is `tokens_before - tokens_freed` and matches the
  `compression applied` line on the same `request_id`. The CTX saving is no
  longer added to a baseline that never contained it. `ctx_offload rewrote
  tool_result blocks` was raised from `debug!` to `info!` (`event =
  "ctx_offload_accounting"`), which is the diagnostic 1d asked for: the two
  lines join on `request_id` and give the per-request split directly.

  Regression test: `sizes_books_only_the_compression_turn_so_tok_after_stays_non_negative`
  (`proxy.rs`), using the live 22:40:36Z numbers from 1e. It pins both the
  correct result and the negative shape the defect produced, so folding the CTX
  term back in fails the test. **Its limit:** it covers `sizes()`, not the
  struct literal that feeds it — that lives inside `forward_http` and is not
  reachable from a unit test without splitting the function up.

  A `compression_accounting_scope` event pairing both halves on the Anthropic
  side was described in an earlier revision but never landed; the edit failed and
  was not retried. The `ctx_offload_accounting` + PERF join above covers the same
  ground, so it is not being re-added.
- **7:** retry warnings now include `retry_after_header`, `delay_source`, and
  `retry_after_clamped`, making header use and the 30-second clamp observable
  without changing retry behavior. The clamp flag is based on parsed header use
  (`delay_source == "header"`), not header presence, so an unparseable header
  cannot be mislabeled as the source of a capped backoff.
- **9:** the detached SSE parser task is now awaited by a second detached waiter.
  Panics and cancellations log a request-scoped error instead of disappearing
  with a dropped `JoinHandle`. The waiter also logs normal completion with the
  number of chunks sent to the parser and chunks dropped because its queue was
  full or closed. This does not yet recover accounting for a task that fails;
  it distinguishes parser failure from upstream/body lifecycle loss so the
  remaining cause can be measured.
- **12.1/13:** added warnings for malformed CCR hashes and CCR tool calls with
  missing identifiers in `headroom-core`. Retrieval behavior is unchanged, but
  failed validation and unmatchable results now leave an operator-visible trace.
- **4 (and partly 3a/5/11):** `volatile_content_detected` now carries
  `session_key_hash` and `conversation_key`, the same two identifiers the drift
  and recache events use, so a volatile finding can be joined to the bust it is
  suspected of causing. Item 4's caveat — "the warning logs no session key, so
  1811 varying is an upper bound" — is now answerable from logs. The session key
  is derived once per request and shared with the drift detector rather than
  re-derived, which would have put six extra SHA-256 digests on the hot path of
  every request.
- **Recache events now carry a session key (2026-08-09).** `begin_request` takes
  it and `PendingRequest` parks it, on both the Anthropic and routed paths, so
  `cache_recache_observed` joins to the drift and volatile events directly
  instead of through time. The value is the drift detector's own
  `session_key_log_prefix(session_key)` — not a re-derivation, and emphatically
  not the earlier attempt's `hash(conversation_key)`, which was the same field
  name holding a different number and joined to nothing. A test asserts the
  field reaches the emitted event, and a second asserts a request that never
  reached the drift gate prints it empty rather than inventing one.
- **9:** SSE parser completion and failure events include sent and dropped chunk
  counts, allowing queue pressure and parser failure to be separated from an
  upstream/body lifecycle gap.
- **7a:** routed-model retries now carry request IDs and the same header/source/
  clamp fields as the main proxy retry path, including transport-backoff source.
- **1e/15:** the routed path splits the same way, and logs both halves under
  `routed_compression_accounting`.

  **Read 1d and 1e together before quoting a savings figure.** Calling the CTX
  value "stale" was too strong, and reading the source does not support it:
  `OffloadOutcome` is built fresh per call (`ctx_offload.rs:237`) and each
  block's saving is a live tokenizer measurement (`:382`). The pinned value in
  1e has a duller explanation — the client re-sends its own history each turn,
  so the same blocks are offloaded again and their identical total is booked
  again. That is a real per-request saving of a cumulative quantity, not a
  counter holding a stale number. What made it wrong was only ever the
  subtraction it fed.

  So the current state understates savings on purpose: a real CTX saving is
  measured, logged, and **not** booked. Expect reported `tok_saved` to drop.
  Restoring it needs 1d's product decision (does the headline figure credit CTX
  offload?) and a baseline that contains it — not another change to this field.
- **4 (fix):** `emit_volatile_warnings` remembers what each location last held
  per conversation and warns only when the value moves. Two details that
  decide whether it works at all: findings at one location within a request are
  judged **as a set** (a docstring with five example dates arrives as five
  findings under one path, and compared one at a time each differs from the one
  before, so every location would look like it was churning); and the first
  sighting still warns, because a conversation that only ever sends one request
  — most subagent traffic — would otherwise never hear from the detector.
  Suppressing the first sighting too was tried and dropped for that reason.
  Memory is a bounded LRU, 256 conversations by 64 locations.
  *Superseded 2026-09-01:* the first sighting reports at INFO as
  `volatile_content_suspected` and no longer warns. See item 1 of
  `TODO-proxy-followups.md` for the measurement that forced it.
- **6 (fix):** a turn that fails upstream still skips the savings, cost and PERF
  sinks — a failed request must not inflate the save-rate — but it no longer
  leaves *no* trace. `emit_request_outcome` emits `request_failed_accounting`
  with the tokens forwarded, the saving that was measured and deliberately not
  booked, the status and the transforms. Both paths get it, since it sits in the
  shared funnel. **Not** booked into the ledger: that denominator change is the
  same product decision as items 2 and 10, and making it here would corrupt the
  number this line exists to let you audit. The item's second question — whether
  the client saw a truncated response or a clean error — is still not answerable
  from proxy logs.
- **10:** savings outcomes now emit fresh-input and cache-read pricing
  counterfactuals with model, token counts, and both dollar values; the durable
  ledger's existing price is unchanged pending a product decision.
- **12.3/12.4/12.7:** semantic-cache hits, misses, TTL expiry, and capacity
  eviction are logged; memory FTS cleanup failures and in-memory CCR capacity
  evictions are now operator-visible.
- **16:** every reformat boundary emits `transform_byte_integrity` with the
  transform name, input and output byte lengths, and a short SHA-256 of each
  side. Run the payload through and the first stage whose output hash you do not
  expect is the one that edited it. No tool-result content reaches the log.

  **It is a `debug!`, so at the proxy's `info` level it fires zero times** —
  the same trap that left 1d unprovable. It carries
  `target: "headroom::pipeline"`, so enable just this one:

  ```
  RUST_LOG=info,headroom::pipeline=debug
  ```

  Field expressions are not evaluated when the level is off (checked against
  `tracing` directly, not assumed), so the two hashes cost nothing until asked
  for. Leaving it at `info` would hash every reformat input and output on the
  hot path, and `opt_ms` is currently in the healthy list.
- **14:** the Anthropic and routed retry warnings carry `session_key_hash`, so a
  retry burst can be joined to the recache events that follow it on the same
  session. That tests the ordering item 14 asserts; it does not assume the clamp
  caused the cold cache. Note the join reaches the *drift* events cleanly and
  the *recache* events only through time, until the gap above is closed.
- **12.2:** CCR context tracker capacity evictions now emit the evicted hash,
  configured capacity, and resulting size.
- **12.3:** CTX offload persistence now logs CCR and FTS outcomes separately,
  including partial persistence.
- **12.6:** `ctx purge` now propagates chunk-delete errors instead of reporting a
  false zero count.
- **12.7:** Codex rate-limit shape misses now emit a warning rather than silently
  returning `None`.

Items 3d and 3e record hypotheses that were tested and **dropped** — read them
for what was ruled out, not as open work. Item 3b is a one-line observation, now
superseded by item 11.

---

## 1. `tok_after` goes negative — 24 of 447 turns (5.4%)

```
msgs=694 tok_before=3899 tok_after=-2171 tok_saved=6070 cache_write=355991
```

`tok_after` is `tok_before - tok_saved`, and savings exceed the baseline.
Ratio of `tok_saved` to `tok_before` runs from 1.5x to 12.1x. Negative
`tok_after` sums to **-70,656** tokens across the run.

**Leading hypothesis: `tok_saved` is sticky across turns.** The 24 turns carry
only **6 distinct `(tok_before, tok_saved)` pairs**:

```
(3899, 6070) x11    (2633, 6610) x5    (4127, 6255) x3
(1984, 6239) x2     (339, 4089)  x2    + 1 more
```

An identical savings figure recurring on 11 separate turns is not 11
independent measurements. It reads as a value computed once when compression
ran, then re-reported on every later turn of that conversation, while
`tok_before` tracks only the current turn's live zone. The two operands are
then from different turns, not merely different scopes — which is why the
subtraction can go negative.

**But re-emission is not the whole story.** A later turn produced a fresh,
non-repeating signature on a *short* conversation:

```
20:42:41  msgs=19  tok_before=6678  tok_saved=29760  tok_after=-23082
```

Nineteen messages, and compression reports saving 4.5x more than the entire
measured baseline. No re-emission can explain a first occurrence. Something
is being counted as saved that `tok_before` never counted as present — which
is the scope mismatch, back again on independent evidence.

**Current reading: two mechanisms, compounding.**

1. `tok_saved` and `tok_before` are measured over different content (scope).
2. `tok_saved` then persists across turns of a conversation (re-emission),
   which is why 25 negative turns share only 7 signatures.

### 1a. `log_compressor` is the main offender

Negative rate by transform. The transform name is on the `compression applied`
line as `strategies`, keyed by request_id; PERF lines don't carry it, so the
two have to be joined. Re-run: `proxy_log_audit.py negtokens`.

| strategies on the turn | negative turns | rate |
| --- | --- | --- |
| `log_compressor` + `search_compressor` | 55 / 111 | **50%** |
| `log_compressor` alone | 26 / 83 | **31%** |
| `search_compressor` alone | 31 / 601 | 5.2% |
| `diff_compressor` alone | 1 / 7 | 14% |
| `config_lossless` (any combination) | 0 / 92 | 0% |

Every combination containing `log_compressor` is far above the rest, and
`config_lossless` never produces one. `search_compressor` at 5% is the
control. That makes `log_compressor` the clearest place to start.

The two largest negatives of the run, both post-idle and both fresh
signatures:

```
20:42:41  msgs=19  before=6678  saved=29760  after=-23082  diff_compressor
20:48:02  msgs=41  before=4058  saved=50276  after=-46218  log_compressor
```

`diff_compressor` produced only one negative all run and it is this one — so
the fault is not confined to `log_compressor`, it is just concentrated there.
Magnitudes are also escalating: the worst pre-idle negative was -3,750, these
are -23,082 and -46,218.

**To investigate — best entry point in this document.** Start at 20:42:41:
19 messages is small enough to reconstruct by hand, unlike the 694-message
cases. Compare how `log_compressor` and `diff_compressor` report savings
against how `tok_before` measures input; `search_compressor` at 3% is the
control. Then check whether either value recurs on later turns of the same
conversation, which would confirm mechanism 2 independently.

### 1b. Both operands freeze across turns

**Cleanest trace of the run — start here.** One conversation, seven
consecutive turns, `msgs` rising monotonically so there is no interleaving
ambiguity, every turn `log_compressor`:

```
21:11:11  msgs=15  before= 643  saved=4426  after=-3783
21:11:14  msgs=17  before=1143  saved=4773  after=-3630
21:11:26  msgs=19  before=1143  saved=4773  after=-3630
21:11:28  msgs=22  before=1143  saved=4773  after=-3630
21:11:33  msgs=24  before=1143  saved=4773  after=-3630
21:11:35  msgs=26  before=1143  saved=4773  after=-3630
21:11:39  msgs=28  before=1143  saved=4773  after=-3630
```

After turn 17 both operands freeze completely and stay frozen. The full run:

```
21 turns  21:11:14 -> 21:12:48   msgs 17 -> 61   before=1143 saved=4773 (identical every turn)
```

Ninety-four seconds, 21 turns, the conversation growing by 44 messages, and
every single turn re-reporting the same two numbers. One 4,773-token
compression was booked **21 times — 100,233 phantom tokens saved** and
**-76,230 of phantom negative** from one real event.

For scale: that single frozen conversation re-books more claimed savings than
the entire first hour's reported total (264,247) is worth trusting, and it did
it in a minute and a half.

This makes items 1, 1a and 1b one story: `log_compressor` computes savings
once, both operands stick, and every later turn re-books them. It also
explains the repeated signatures in item 1 (`(3899, 6070)` x11) without
needing a separate mechanism.

Earlier corroboration from a different conversation, where the freeze was
harmless because the operands stuck at near-equal values:

```
20:40:59  msgs=10  before=23259  saved=23257  after=2
20:41:09  msgs=12  before=23259  saved=23257  after=2
```

Negatives appear only when `tok_before` is recomputed smaller while
`tok_saved` holds its older, larger value.

### 1c. Correction: `tok_saved` *is* cumulative

An earlier revision of this document rejected the accumulator reading, on the
strength of a flat segment and a time ordering that interleaved several
conversations. **That was wrong.** Tracking one conversation across 35
consecutive `log_compressor` turns:

```
msgs=15  before= 643  saved= 4426
msgs=17  before=1143  saved= 4773
msgs=65  before=1602  saved= 5089
msgs=75  before=1602  saved=11094
```

`tok_saved` is **monotonically non-decreasing across all 35 turns** — verified,
no decreases. `tok_before` takes only three distinct values in the same span
(643, 1143, 1602).

**The actual mechanism.** Both are step functions over the conversation, and
`tok_saved` climbs faster than `tok_before`:

- `tok_saved` is a running total of every compression so far in the conversation.
- `tok_before` updates rarely and stays small.
- The "freeze" described above is simply the flat stretch between compressions.
- `tok_after` goes negative once the cumulative total overtakes the stale baseline,
  and cannot recover afterwards — it only gets worse as the conversation runs on.

This means the defect is not confined to negative turns. Every turn after the
first compression re-reports a cumulative figure as if it were that turn's
saving, so the run-total in item 2 double-counts across the whole corpus, not
just the 3% that go negative.

**Proof that needs no conversation key.** The grouping problem above made this
harder than it is. Each compressed turn emits *two* lines under one
`request_id`: `compression applied`, carrying that turn's own `tokens_before`
/ `tokens_after` / `tokens_freed`, and the PERF line carrying `tok_before` /
`tok_saved`. Joining them on `request_id` settles the question outright.
Re-run: `proxy_log_audit.py cumulative`.

Over 1,049 requests that emit both lines:

- 930 agree — `tok_saved` equals that turn's `tokens_freed`.
- **119 disagree, and every single one is upward.** Not one downward
  disagreement in the corpus.

The signature behind item 1's most common negative resolves exactly:

```
compression applied:  tokens_before=3899  tokens_freed=2513   (this turn)
PERF line:            tok_before=3899     tok_saved=6070      (running total)
```

So `tok_after` should read 3899 − 2513 = **1386**. It reports 3899 − 6070 =
**−2171**. The per-turn truth is already in the log, one line away from the
figure that gets published.

This also narrows the fix: `compression applied` computes the right number,
so the defect is in what PERF reads, not in what the compressors report.

### 1d. Root cause located — `proxy.rs:3144-3147`

The outcome is built with a baseline and a saving drawn from different scopes:

```rust
original_tokens: compress_tokens_before,        // 3144
// Compression's own saving plus anything the CTX transforms
// removed before it ran.
tokens_saved: compress_tokens_saved + ctx_transform_tokens_saved,  // 3147
```

`compress_tokens_before` is measured *after* the CTX transforms already shrank
the body (`ctx_transform_tokens_saved` accrues at line 2619, before compression
runs at 3069). Line 3147 then adds that earlier saving back in. The saving is
booked against a baseline that never contained it, so `saved` can exceed
`before` — which is exactly the shape items 1 and 1c measure.

PERF prints three independent struct fields (`request_outcome.rs:386-408`);
`tok_after` is `optimized_tokens`, resolved by `sizes()` at `proxy.rs:4652`:

```rust
self.original_tokens.saturating_sub(self.tokens_saved)   // 4656
```

**`saturating_sub` does not clamp at zero here.** These are `i64`, so it
saturates at `i64::MIN` and passes the negative straight through. Anyone
skimming that line will read it as already-guarded. It isn't.

**Empirical fit.** Across the run, 151 negative rows all have a matching
`compression applied` line, and the excess (`tok_saved` − that turn's
`tokens_freed`) is stable per conversation while `tok_before` moves
independently:

```
tok_before=1143  excess= 3936  x22
tok_before=1602  excess= 9941  x18
tok_before=5503  excess=13782  x17
tok_before=3899  excess= 3557  x11
```

A per-turn bug would give excesses that track `tok_before`. A saving carried
across the conversation and rebooked against one turn gives exactly this.

**Not fully proven.** Attributing the excess to `ctx_transform_tokens_saved`
directly needs the `ctx_offload rewrote tool_result blocks` line
(`proxy.rs:2624`), which is `debug!` — the proxy runs at info, so it appears
zero times in the log. Raise that one line to `info!`, or log
`ctx_transform_tokens_saved` on the PERF line, and the attribution becomes a
direct read rather than an inference.

### 1e. The excess is re-emitted, so 1d's fix is necessary but not sufficient

**Measured after the 1d fix was written, and it changes the diagnosis.** Over
1,396 joined turns since 20:00Z, 139 carry a positive excess — and those 139
hold only **10 distinct excess values**, a mean of 13.9 repeats each. Grouping
by value shows what each one is:

```
excess=13782  x42  21:36-21:49  distinct msgs=41 (66..154)  distinct tok_before=5
excess= 9941  x30  21:13-21:18  distinct msgs=30 (75..149)  distinct tok_before=2
excess= 3936  x28  21:11-21:13  distinct msgs=28 (15..73)   distinct tok_before=3
excess=12197  x10  22:40-22:42  distinct msgs=10 (58..77)   distinct tok_before=1
```

Each value tracks **one conversation**, pinned across 41 different message
counts and 13 minutes while the conversation grows underneath it. A genuine
per-turn CTX offload saving would vary with what each turn offloaded. This one
is computed once and re-reported — item 1's second mechanism, which item 1
called re-emission and 1d set aside.

The 22:40 case shows why it matters. `tok_before=358`, compression freed 243,
excess 12,197 — the fix computes `358 + 12197` as the baseline, so `tok_after`
lands at 115 and the negative disappears. **The arithmetic is right and the
number is still wrong**, because a 358-token compressible body did not have
12,197 tokens removed from it by CTX transforms on that turn. The fix makes the
symptom vanish while booking a saving that turn never made.

`ctx_transform_tokens_saved` is declared per-request at `proxy.rs:2345`, so the
proxy is not accumulating it across turns. The stale value therefore comes from
something upstream of that counter — the offload runtime returning a
conversation-level total in `out.tokens_saved` (`proxy.rs:2619`) rather than
this turn's delta, most likely. That is the next thing to read.

**Do not treat item 1 as closed when negatives stop appearing.** Negatives are
the visible tail of the re-emission; the fix removes the tail. Verify instead
that the excess *varies per turn* — re-run the grouping above and check that
distinct excess values roughly equal the number of turns.

**Fix direction** (decide before touching): either raise the baseline to
pre-transform size, or book only `compress_tokens_saved` here and account for
the CTX saving separately. They give different savings figures — the first
credits the proxy for CTX offload, the second doesn't. That is a product
question, not a bug fix, and it also moves item 2.

**Held up on a fresh case.** A negative turn at 22:24:25Z, on a different
transform and outside the original sample:

```
PERF line:            tok_before=5914  tok_saved=11134   -> tok_after=-5220
compression applied:  tokens_before=5914  tokens_freed=5627  strategies=["search_compressor"]
```

`tokens_before` matches PERF's `tok_before` exactly, so the baseline is shared
and only the saving diverges — the excess is 5,507 and the correct `tok_after`
is 287. That is 1d's prediction with no fitting: one turn's compression freed
5,627, and PERF reported nearly twice that.

Also a useful rate check. That hour ran 191 PERF turns with **1** negative
(0.5%), against 3.0% in the original window. The defect is not gone — it
surfaces only once the accumulated saving overtakes a small baseline, so a
window of large turns hides it. Do not read a low negative count as
improvement.

`search_compressor` here, `log_compressor` in item 1a: the defect is in the
shared booking path, not in any one transform, exactly as 1d locates it.

PERF lines still carry `request_id` but no conversation key. That is what made
the earlier ordering mistake possible and is worth fixing on its own — but it
is no longer a blocker for this item.

Also note `tok_after=2` recurs as an apparent floor whenever `tok_saved`
approaches `tok_before`. Worth confirming it is a deliberate placeholder and
not a clamp masking further negatives.

Running totals: 25 of 842 turns (3.0%), negative `tok_after` summing to
**-93,738**.

## 2. Savings baseline undercounts by roughly 100x

Totals across 311 turns:

| metric | tokens |
| --- | --- |
| `tok_before` | 284,278 |
| `tok_after` | 20,031 |
| `tok_saved` | 264,247 |
| `cache_read` | 34,654,500 |
| `cache_write` | 2,125,211 |

`tok_before` covers under 1% of what crosses the wire. Any savings percentage
derived from `tok_before`/`tok_after` describes the live zone, not the request.

If item 1's re-emission hypothesis holds, the 264,247 `tok_saved` total is
inflated on top of that — the same savings counted once per subsequent turn.
**Both the savings total and any ratio built on it should be treated as
unverified until item 1 is settled.**

**To investigate:** decide what the headline savings number is meant to mean,
then make the two operands share both a scope and a turn. Relates to the
recent "count what the proxy costs, not just what it saves" work.

## 3. Proxy spent more than it saved during the window

**The numbers below are superseded — do not quote them.** Item 11 settled as an
artefact on 2026-08-09: a share of this "waste" was one stream being charged
for another stream's prefix under a merged key. On the fresh window, 68% of
booked tokens sat on keys carrying interleaved streams. The
stream-matching fix removes that class of event, so both the waste total and
the spend-to-save ratio need re-deriving on a run of the new binary. The
conclusion "spent more than it saved" is unproven until they are.

| | tokens |
| --- | --- |
| `tok_saved` (to 11:40) | 264,247 |
| recache waste, genuine drift (to 12:15) | 505,052 |
| recache waste, classified expected (to 11:40) | 534,485 |

Excluding the expected bucket, real waste outweighs savings. Genuine drift is
overwhelmingly `early_messages`.

Not a startup artefact and not settling: waste has kept accruing well past the
original window — 58,368 at 11:42:09, 22,997 at 11:49:10, 130,624 at 12:15:33.
Events over 40K tokens dominate the total, so the tail matters more than the
event count.

**Both operands are suspect.** Savings may be inflated by re-emission
(item 1); waste may be inflated by double-counting (3a) and by cold starts
misbooked as drift (3c). The direction of the imbalance has held across every
sample so far, but no ratio here should be quoted until 1, 3a and 3c are
settled.

**To investigate:** whether the `system` and `early_messages` drift is
self-inflicted (injection, prefix replay) or client-driven.

### 3a. Possible double-counting of recache waste

The 12 genuine-drift events include two pairs recorded a second apart with
byte-identical waste but different request IDs:

```
11:39:48  early_messages  waste=8132   req=e74bd1a9
11:39:49  early_messages  waste=8132   req=9afc1292
11:41:35  early_messages  waste=57059  req=883e8731
11:41:35  early_messages  waste=775    req=ed47cb9a
```

Suspicion: concurrent requests on one conversation each book the same cache
bust. If so the waste totals in item 3 are inflated and the "spent more than
it saved" conclusion needs re-deriving after de-duplication. **Treat the
1.3x ratio as provisional until this is settled.**

The same shape shows up in PERF lines: 11:50:56 and 11:50:57 both booked
`tok_before=339 tok_saved=4089 tok_after=-3750`. Whatever duplicates recache
events may duplicate turn accounting too — check both together.

### 3c. Cold starts may be booked as drift — CLOSED, premise does not hold

**Checked in source 2026-08-09; not a defect.** `observe_drift`
(`drift_detector.rs`) returns `None` on a session it has not seen, so
`drift_dims` is `None` and the recache is classified `Expected`, never `Drift`.
A first request cannot produce a drift-kind event about itself. The correlation
below pairs one request's recache event with a *different* request's
`first_request` line — the two detectors use different keys and different
lifetimes, so proximity in the log means only that both were busy.

The live event that prompted the re-check (conv `441fcd3312c6`, 41,023 tokens,
`actual_cache_read=0`) carried `drift_dims=tools,early_messages`: the drift
detector independently saw the bytes move. Its `expected_cache_read` came from a
previous turn that really did write 41,023 tokens into the cache. Tokens paid
for a cache write that is never read are genuinely wasted, so booking them is
right.

What *was* wrong in this area is item 11 — where the "previous turn" belonged to
another stream — and that is fixed separately. The tail-heaviness this item
notes is real and still unexplained; it is not explained by cold starts.

**Original write-up follows.**

The largest single event of the run:

```
12:15:33.884  cache_recache_observed  drift_dims=early_messages
              wasted_tokens=130624  conversation_key=e627d0108d17120a
12:15:34.433  cache_drift_first_request  session_key_hash=a387126f...
```

The detector declares this a *first request* — a session it has never seen —
half a second after 130,624 tokens were booked against it as `early_messages`
drift. Nothing can drift from a prefix that was never recorded.

**Prediction made, then confirmed.** This was written up after the 12:15:33
event; the next large event was expected to show the same shape. It did:

```
21:06:18.987  cache_recache_observed  121,188  early_messages  conv=ddb4d83412
21:06:21.009  cache_drift_first_request                        session=efc0c544fa
```

2.0s apart, recache again emitted *before* the first_request line. The two
largest events of the run (130,624 and 121,188) both match.

Across the full run, 19 genuine-drift events totalling 639,391 tokens:

- **14 of 19 sit within 120s of a `cache_drift_first_request`** — 511,322
  tokens, **80% of all waste**.
- Waste is concentrated in the tail: 7 events over 40K account for 571,582
  tokens, **89% of the total**.

**Strength of evidence:** good and improving — it survived a prediction rather
than only fitting past data. Still not universal: 59,750 at 11:13:28 and
58,368 at 11:42:09 have no first_request within 120s, so at least one other
mechanism produces large events.

Because the waste is so tail-heavy, fixing only the cold-start path would
remove most of the reported waste without touching most of the events.

**To investigate:** whether a conversation with no recorded prior prefix can
reach the recache path and, if so, what it compares against. Check ordering
too — the recache event is emitted *before* the first_request line.

Note this one had low concurrency (7 forwarded requests in the surrounding
two minutes, 1 compression), so it does not fit 3b.

### 3d. First `tools` drift — the skipped sort may have a price

At 21:10:48, the only `tools` drift of the run:

```
drift_dims=tools,early_messages  wasted=41,460  conv=026d3f272b
```

Full breakdown at this point (20 events, 680,851 tokens):

| drift_dims | events | tokens |
| --- | --- | --- |
| `early_messages` | 17 | 484,097 |
| `system,early_messages` | 1 | 95,544 |
| `system` | 1 | 59,750 |
| `tools,early_messages` | 1 | 41,460 |

This is the first sign that the tool array changing between turns costs real
tokens. It connects to the per-request log line `tool-array sort skipped:
non-PAYG auth mode passes through byte-equal` (item 8): the proxy deliberately
does not normalise tool order for subscription auth, so a client that varies
its tool set or ordering busts the prefix and nothing intervenes.

**Two candidate explanations — test run, tool-ordering not supported.** A
second `tools` event arrived at 21:22:47 (15,688 tokens). Both sit beside a
`cache_drift_first_request`:

```
21:10:48  41,460  tools,early_messages  conv=026d3f272b  first_req  +5.9s
21:22:47  15,688  tools,early_messages  conv=a5eaf1fc54  first_req -12.9s
```

Neither occurs independently of a cold start, so **3c explains both and the
skipped tool-sort explains nothing extra so far.** Do not act on the
tool-ordering theory without an instance that has no cold start nearby.

A further hint that these are 3c: `tools` never appears alone, only paired
with `early_messages`. Several dimensions reporting drift at once is what you
would expect when there is no stored prior to compare against — everything
looks changed. Worth checking whether the detector reports all dims on a
first sighting.

### 3e. Large `system` drift, not self-inflicted

At 21:21:35, 122,547 tokens on `system` drift alone — third-largest event of
the run, and the largest `system`-only one:

```
21:21:23.097  cache_drift_observed    drift_dims=system  session=48f481c4
                previous=180833db036e10a18b097e84  current=c9d1bea9e7fbe758f8525d80
21:21:35.544  cache_recache_observed  drift_dims=system  conv=1034467d79f972c6
                wasted_tokens=122547  expected_cache_read=150022
```

82% of the expected cache read was lost to a system-prompt change.

**Hypothesis raised and dropped: proxy injection.** The proxy runs with
`--ctx-inject=true`, so injected content varying per request would be a
self-inflicted cause. The two-minute window around this event contains **no
injection events at all**, and no `cache_drift_first_request`, so it is
neither injection nor 3c. The system prompt genuinely changed between turns
on the client side.

**To investigate:** what legitimately changes a system prompt mid-conversation
in Claude Code — a subagent with different instructions, or a skill loading —
and whether the proxy can keep those on separate cache keys instead of
letting them overwrite one another.

Note the two events use different identifiers (`session_key_hash` vs
`conversation_key`) and sit 12s apart, so joining them from logs takes care.

### 3b. Drift clusters under concurrency

10 of 12 genuine-drift events are `early_messages`, and 8 of those land
between 11:39 and 11:42 — the same three minutes where throughput peaked
(ok turns per minute: 23, 32, 24, 16, against a run median near 5).

Suspicion: parallel requests on one conversation interleave and each sees the
other's prefix. Same suspected mechanism as item 5.

## 4. Volatile-prefix warning fires on static values

**DECIDED AND FIXED 2026-08-09.** The caveat below — "1811 varying is an upper
bound" — is now answerable, and the answer is that **nothing varied**. Grouping
the live warnings by `(conversation_key, location)` and comparing the sample
set per request: 37 sites seen in two or more requests, **zero** of them ever
changed value, 4,004 warnings between them. Fix (a) is implemented; see the
fixes section. Note this also disposes of the shape of the original count —
"825 come from values that never change" was itself an undercount, because
several samples at one location within a single request were being read as
change over time.

2455 warnings in 67 minutes, 7.2 per request — 43% of all log lines this run.

Tested by collecting distinct samples per location: **825 warnings come from
values that never change once.** Every `tools[].input_schema` hit is the
literal string `2026-07-14T10:05:00` in a `from.description`; placeholder
UUIDs like `22222222-2222-4222-a222-222222222222` also recur verbatim.

The detector matches on shape ("looks like an ISO timestamp") rather than
diffing what it saw last time. Static example text in a tool docstring cannot
bust a cache.

Meanwhile measured `cache_hit_pct` is **99 median, 92.7 mean** — the failure
the warning predicts is not happening at anything like this rate.

**Caveat:** the warning logs no session key, so for the 12 locations whose
samples do vary I could not separate per-session churn from ordinary
cross-session difference. 1811 "varying" is an upper bound, not a count.

**To investigate:** (a) suppress when the value at a location is unchanged
from the previous request on the same conversation; (b) add a session key to
the warning so this is answerable from logs. Note the drift detector only
hashes system, tools and the first 3 messages — open question in
TODO-recache-classification.md about widening that window is related, since
the busiest locations here are `messages[3]`, `[6]`, `[13]`, `[38]`.

**Source.** Emitted by `emit_volatile_warnings`
(`cache_stabilization/volatile_detector.rs:155-168`), message text at :163-165.
Detection runs in `detect_volatile_content` (:143-150) walking the body via
`walk_anthropic` / `walk_openai` (from :172); the `VolatileKind` variants
`Timestamp` / `Uuid` / `IdField` are at :68-92. Confirmed by reading: the
detector matches shape only and holds no previous request to diff against, so
fix (a) needs new state, not a new condition.

## 5. Cache-drift flip-flop between exactly two prefixes

**SETTLED 2026-08-09 — this is item 11, and it is fixed there.** Of the three
suspicions below, the third was right: two streams share one key. Live proof is
`af7a42fd7eb2`, which alternates between exactly two prefixes and resolves into
two normally-growing conversations once the message count is read alongside the
hash. "Never a third hash" is the signature of two interleaved streams, not of
drift. Left in place below for the record.

Two sessions alternate perfectly, never a third hash, always `early_messages`:

```
session 97f195df03  54 drifts, 2 prefixes:  4d2a2630 -> d1913774 -> 4d2a2630 -> ...
session 9058166346  27 drifts, 2 prefixes:  f7cd246f -> cb2212e5 -> f7cd246f -> ...
```

Gradual drift does not look like this. Two request shapes are sharing one
`session_key_hash`.

**Suspicions, untested:** two clients colliding on one session key; or a
transform applied on alternate requests only; or main-agent and subagent
traffic hashing to the same key.

## 6. Failed turns are excluded from accounting

**PARTLY FIXED 2026-08-09.** Failed turns now log what they forwarded
(`request_failed_accounting`), so the cost of failure is countable instead of
invisible. They are still not booked into the ledger — that is a deliberate
product decision left open, shared with items 2 and 10.

At 11:46 a log line appeared that had not occurred all run:

```
11:46:19  upstream error inside a 200 stream survived every retry
```

Per-minute, having been zero for the whole preceding hour:

```
11:14–11:44   retry 0–2/min, EXHAUSTED=0, dropped=0
11:46         ok=5  retry=5  EXHAUSTED=2  dropped=2
```

The upstream `overloaded_error` is capacity on Anthropic's side, not a proxy
bug (same conclusion as the 2026-07-07 note in the recache doc). The
accounting consequence is ours: each exhausted request ends with *"usage is
partial, so this turn is not booked into cost or savings"*.

So the turns that fail are exactly the turns that leave no trace in the
ledger, while having cost real upstream tokens — possibly three times over.
Under load the savings figures improve as behaviour worsens. This compounds
items 1 and 2.

**To investigate:** book a partial/failed turn under its own category rather
than dropping it. Also check whether the client saw a truncated response or a
clean error — not answerable from proxy logs alone.

**Source.** The "survived every retry" line is `proxy.rs:3634`. The drop
decision is upstream of it in `emit_request_outcome`
(`request_outcome.rs:351-359`): status >= 500 calls `record_failed` and returns
before the savings, cost and PERF sinks run. That early return is the whole of
this item — a failed turn never reaches the ledger by construction, so fixing it
means giving `record_failed` its own accounting, not relaxing the guard.

## 7. 429 retries don't *log* `Retry-After` (corrected)

**This item said "429 retries ignore `Retry-After`". That was wrong.** The
header is read and honoured. Correction made after reading the source; the
original claim rested on the header never appearing in the log, which measures
log coverage, not behaviour.

`proxy.rs:3543-3564` parses `retry-after` as numeric seconds, then falls back to
an RFC 2822 date, and clamps to `retry_max_delay_ms`. Line 3565 uses it as
`delay_ms`, dropping to jittered exponential backoff (`retry_base_delay_ms`,
1000ms, `config.rs:1963`) only when the header is absent or unparseable.

The real gap is narrow: the warning at `proxy.rs:3579-3586` logs `attempt`,
`status`, `max_attempts` and the final `delay_ms`, but never whether the header
was present or used. So the log cannot distinguish "server said 500ms" from
"server said nothing and backoff computed 500ms".

Observed delays:

```
429:        500, 620, 830, 980, 1140, 1660, 2720 ms
overloaded: 1000 x6, 2000 x2 ms
```

The 620/830/1140/2720 values carry jitter, so those came from local backoff. The
flat 500 is suspicious: base is 1000ms and jitter spans 50-150%, so a first
attempt cannot produce 500ms — the minimum is 500ms exactly, at jitter=50. Not
conclusive.

**A second window answers it without touching the code.** All 429s on the
evening of 2026-08-08 (UTC):

```
21:10:38  attempt=1  delay_ms=1010     21:10:39  attempt=2  delay_ms=2480
21:22:34  attempt=1  delay_ms=1350     21:22:36  attempt=2  delay_ms=1320
21:38:13  attempt=1  delay_ms= 780     21:38:15  attempt=2  delay_ms=1180
22:06:21  attempt=1  delay_ms= 960     22:06:22  attempt=2  delay_ms=2140
```

The header path returns `secs * 1000`, so honouring it yields a round multiple
of 1000. **Not one of these is round**, and every attempt=1 value sits inside
[500,1500] — exactly `base=1000` under 50-150% jitter. Local backoff computed
all eight, so `retry-after` was absent or unparseable on every one.

This does not weaken the fix. The header is honoured *when sent*; upstream
simply is not sending it here, which is a fact about Anthropic's 429s worth
knowing before anyone builds rate-limit handling on the assumption it arrives.
The missing log field is what forced this roundabout inference from jitter
arithmetic.

**Superseded 40 minutes later — the header does arrive, under real rate
limits.** A genuine session-limit burst at 22:16-22:18Z:

```
22:16:38  attempt=1  delay=30000  rid=3387a58b
22:16:47  attempt=1  delay=30000  rid=de84ef82
22:17:10  attempt=2  delay=30000  rid=3387a58b
22:17:20  attempt=2  delay=30000  rid=de84ef82
22:17:53  attempt=1  delay=30000  rid=8c04349a
22:18:26  attempt=2  delay=30000  rid=8c04349a
```

Every value is exactly 30000, which is `retry_max_delay_ms`
(`config.rs:1964`). Backoff cannot produce it: attempt index 0 gives
`1000 × 1` jittered to 500-1500, index 1 gives `1000 × 2` jittered to
1000-3000. Landing on 30000 exactly, six times, requires the header path —
`(secs * 1000).min(max_delay)` with `secs >= 30`.

So the earlier reading was drawn from the wrong sample. The 21:xx 429s were
incidental and carried no header; a real rate limit sends `retry-after` with a
long value, and the proxy honours it and clamps to 30s. **The clamp is now the
question worth asking:** if upstream says wait 60s and the proxy waits 30s, it
retries into a still-closed window and burns an attempt. Both retries here were
clamped, and all three requests exhausted their attempts.

Two lessons for this document. The header is read, honoured, and clamped —
three separate behaviours, none of them visible in the log, which is exactly why
this item flipped twice. And a quiet window is not a representative sample:
rate-limit behaviour can only be observed while being rate-limited.

**`delay_ms` distinguishes the two kinds of 429 on sight.** A later burst at
22:30:25Z read `delay=1280` then `delay=2700` — jittered, so local backoff, so
no header. Compare the 22:16Z session limit, six flat 30000s. Until the log
field lands, this is the field test:

| `delay_ms` | means |
| --- | --- |
| exactly 30000 | header present, value >= 30s, clamped — a real limit |
| jittered, in [500,3000] | no usable header — an incidental 429 |

Useful when reading old logs, and it means the two kinds should never be pooled
into one retry statistic.

### 7a. There is a second retry path, and the fix does not cover it

Found live at 22:56:43Z when the codex upstream returned 503 — the first
non-429/529 retry of the run:

```
22:56:43  event=local_model_upstream_retry  status=503  attempt=1  backoff_ms=1000
22:56:44  event=local_model_upstream_retry  status=503  attempt=2  backoff_ms=2000
```

`handlers/local_model.rs:1365-1389` retries independently of the `proxy.rs`
loop, with its own header parsing. Three differences that matter:

1. **It has none of the new fields.** The item 7 fix added
   `retry_after_header`, `delay_source` and `retry_after_clamped` to
   `proxy.rs:3582` only. This path still logs just status, attempt and
   `backoff_ms`, so the blind spot item 7 exists to close is still open on
   every request routed through a model route.
2. **No RFC 2822 fallback.** It parses numeric seconds only
   (`:1366-1370`); `proxy.rs:3554` also accepts an HTTP-date. A date-format
   `Retry-After` is silently ignored here and falls through to backoff.
3. **No jitter** (`:1373-1377` — plain `base * 2^(attempt-1)`). So **the
   field test in item 7 above does not apply to this path.** Round values like
   1000/2000 are ordinary backoff here, not evidence of a header. Reading
   `local_model_upstream_retry` lines with the proxy.rs table would get the
   diagnosis backwards.

It also clamps to `max_delay_ms` (`:1378`), so item 7's clamp decision applies
to both paths and should be made once.

The warn carries no `request_id`, so these retries cannot be joined to the turn
they belong to — same gap as `model_route_translate` in item 15.

**Fix:** add `retry_after_header`, `delay_source` (`header` | `backoff`) and a
`clamped` flag to the warn at `proxy.rs:3579`. One line, no behaviour change,
and it makes all three behaviours answerable. Note `proxy_log_audit.py retries`
reports `retry_after seen anywhere: 0` — that is the missing log field, not
missing behaviour.

**Separately, decide the clamp.** `retry_max_delay_ms` (30s) silently overrides
a longer server instruction. Honouring a 60s `retry-after` means holding the
request 60s; clamping means a near-certain wasted retry. Under a session limit,
which is when this fires, the server value is the only one that can be right.
See item 14: the same burst was followed by three full prefix rewrites costing
326K tokens, so the clamp may be buying a faster retry at the price of a cold
cache.

## 9. Blind spot: 12% of requests have no completion record at all

Found by reconciling counters that must agree, rather than by reading any
single metric. Every item above came from instrumented values — this one came
from what the instrumentation never says.

```
live-zone dispatch : 1515
outbound_body_bytes: 1511
forwarded          : 1559
sse stream closed  : 1334
PERF booked        : 1335
```

180 dispatched requests (11.9%) are never booked. Their last log line:

```
175  forwarded          <- then nothing. no close, no error, no warning
  4  stream_incomplete
  1  outbound_body_bytes
```

**175 requests are sent upstream and then vanish from the log entirely.**
There is no event for whatever happened next — no client-disconnect event, no
abort, no timeout. The proxy simply stops writing about them.

Rate varies far too much to be background noise:

| hour | unbooked / total | rate |
| --- | --- | --- |
| 10Z | 14 / 182 | 7.7% |
| 11Z | 34 / 352 | 9.7% |
| **12Z** | **81 / 245** | **33.1%** |
| 20Z | 40 / 389 | 10.3% |
| 21Z | 15 / 366 | 4.1% |

They also behave differently from booked requests — reaching `forwarded`
in 0.68s median against 1.66s, so they are systematically smaller or simpler,
not a random sample.

**Why it matters.** These requests were forwarded, so they cost upstream
tokens. None of that reaches cost or savings accounting. Combined with item 6
(exhausted retries also unbooked), the ledger is blind to every request that
does not finish cleanly — and those are precisely the expensive failure modes.
Item 2's totals are computed over the 88% that succeed.

### 9a. Leading hypothesis — the discarded `JoinHandle` at `proxy.rs:3898`

**Structural inference, not a confirmed trace.** Every booking after
`forwarded` happens inside `run_sse_state_machine`, which is launched detached:

```rust
tokio::spawn(run_sse_state_machine(...));   // 3898 — handle never bound
```

The `JoinHandle` is dropped on the spot, so nothing ever awaits it. If that task
panics, tokio catches the panic at the task boundary and stores it in the handle
nobody holds. No log line, no metric, no `stream_incomplete` — the request is
forwarded and then goes quiet. That matches the observed signature better than
client disconnect, which would leave a close event.

It also explains the shape of the anomaly: a panic on a code path taken by
smaller or simpler requests fits both the 0.68s-vs-1.66s timing split and the
33% spike in the 12Z hour, where a burst of similar requests would hit the same
path repeatedly.

**Confirm before fixing** — this predicts a panic, so look for one:

1. Bind the handle and `tokio::spawn` a waiter that logs `JoinError`, splitting
   `is_panic()` from `is_cancelled()`. Cheapest decisive test.
2. Or install `std::panic::set_hook` at startup to log panics with the request
   id in scope.

If neither fires, the hypothesis is wrong and the next suspect is the channel:
`tx` is returned at 3907 and the state machine ends when the sender drops, so an
early drop on the forwarding side would also end the task silently — with no
panic to find.

### 9b. The two unbooked categories are not the same failure

A live `stream_incomplete` at 21:54:36Z was traced end to end. Its full
lifecycle is present in the log:

```
21:54:34.449  anthropic live-zone dispatch
21:54:34.503  outbound_body_bytes
21:54:35.546  forwarded
21:54:36.544  sse stream closed
21:54:36.544  stream ended without message_stop; usage is partial, not booked
              (partial_input_tokens=2, partial_output_tokens=6)
```

**The SSE task ran to completion and logged.** So the 4 `stream_incomplete`
requests are a *different* failure from the 175 that stop after `forwarded`, and
9a's panic hypothesis applies only to the latter. When the state machine runs it
leaves a trace; the 175 leave none, which is what makes "the task never got
there" the right shape of explanation for them.

It also looked at first like a reason to downgrade items 6 and 9: this unbooked
turn cost 2 input and 6 output tokens, and the silent requests reach `forwarded`
faster than booked ones (0.68s vs 1.66s median), suggesting they might all be
small. **Measured, and they are not.**

Joining `anthropic live-zone dispatch` against PERF on request id, and sizing
each by `bytes_out` from `outbound_body_bytes`:

```
dispatched 2524   booked 2118   unbooked 406 (16.1%)
booked    n=2118  bytes_out 631,542,327  median 289,834
unbooked  n= 406  bytes_out 129,326,273  median 279,509
```

Unbooked requests are **17.0% of all wire bytes sent upstream** against 16.1% of
requests — very slightly *larger* than average, with a near-identical median.
The faster time-to-`forwarded` does not mean smaller bodies.

So roughly a sixth of everything the proxy sends upstream is missing from cost
and savings accounting, and it is a representative sixth, not a tail of trivial
calls. Every ratio in this document computed over booked requests — item 2's
totals, item 3's waste-versus-savings comparison, item 10's pricing — is drawn
from 83% of the traffic while describing 100% of it.

Reproduce with the join above; `bytes_out` is the post-transform size actually
put on the wire, which is the right basis for cost. The 2-token example is a
real member of this set, just not a typical one.

### 9c. All three incompletes share one signature: `input_tokens=2`

Every `stream_incomplete` on 2026-08-08 evening:

```
21:14:14  rid=095e7892  in=2 out=2  cache_write=66872  cache_read= 22238  blocks=1
21:54:36  rid=3408370b  in=2 out=6  cache_write=  700  cache_read=100346  blocks=1
22:16:33  rid=3bb2eb17  in=2 out=2  cache_write=  872  cache_read= 80875  blocks=0
```

All three: `upstream_status=200`, `stop_reason=""`, bodies of 224-270KB, and
`input_tokens` of exactly **2**. A 234KB request does not have 2 input tokens.

The cache counters explain it and are the important part: 22K-100K of
`cache_read` plus up to 66K of `cache_write` per request. Anthropic reports
cached input separately from `input_tokens`, so 2 is the uncached remainder —
the same accounting quirk already documented at `proxy.rs:4663-4671`, where
`attempted_input_tokens` collapsed to 8,059 against 3.66M of real input.

**These turns are not cheap.** Item 9b used the 2-token figure to suggest the
`stream_incomplete` set might be trivial; it isn't, and the corrected byte
measurement in 9b already showed why. Each carries real cached-token cost that
never reaches the ledger.

`blocks=0` on the third is worth a look on its own — the stream closed having
produced no content block at all, yet returned 200.

**Do not use `input_tokens` as a size or cost proxy anywhere.** On a warm
conversation it measures the uncached tail and nothing else. Use `bytes_out`,
or sum the cache counters.

**Most likely explanation, untested:** client-side cancellation. Claude Code
aborts in-flight requests routinely. That would make the *behaviour* normal
and the *silence* the bug — a cancelled request still burned upstream tokens.

**To investigate:** add an event when a client disconnects or a request is
dropped, with whatever usage is known at that point. Until then, no statement
about proxy cost or savings covers this 12%. Check the 12Z spike separately:
one hour at 33% suggests a condition, not a constant.

## 10. Savings are priced at fresh-input rates on a 99%-cached workload

Also found by reconciliation, against the on-disk ledger rather than the log.
`~/.headroom/savings_events.jsonl` and `proxy_savings.json` are a second,
independent accounting of savings, fed per-compression rather than per-turn.

Confirmed pricing basis — the ledger values saved tokens at each model's
**fresh-input** rate, never at a cache-read rate:

```
saved=12007  cost_usd=0.180105       12007 x $15/M = 0.180105  exact match
same tokens at cache-read $1.50/M  = 0.018010
```

Dividing `cost_usd` by `saved` across the whole ledger recovers the rate table
in use (`proxy_log_audit.py ledger`):

```
$15.00/M  3249 events      $3.00/M  1682 events
 $1.25/M   882 events      $2.50/M   268 events      $1.00/M  26 events
```

Every one of those is a published *input* price for some model; not one is a
cache-read price. So the fault is not "everything is priced as Opus" — the
per-model lookup works correctly. It is that the lookup asks for the wrong
column.

Measured `cache_hit_pct` on this workload is **99 median, 92.7 mean** (item 4).
Content that compression removes from a stable prefix would overwhelmingly
have been *cache reads*, billed at roughly a tenth of fresh input. Pricing
those removals at full rate overstates the money saved by close to 10x on the
cached portion.

Second, the same compression is booked repeatedly. This run:

```
983 savings events, 3,664,409 tokens claimed
111 distinct (before,after) pairs   -> 8.9 events per distinct compression
counted once each:  868,049 tokens  -> booked total is 4.2x that
```

Repeat counting is defensible on its own — the client re-sends history each
turn, so each request really is smaller. But it compounds with the pricing
problem: the more turns a conversation runs, the more often the same removal
is re-booked, and every re-booking is priced as if it were uncached.

**User-facing numbers built on this** (read live from `proxy_savings.json`):

```
display_session  requests 761     tokens_saved 3,827,391   compression_savings_usd $45.31
lifetime         requests 75,037  tokens_saved 52,139,048  compression_savings_usd $247.92
```

**To investigate — highest value item in this document.** Decide the
counterfactual the savings figure is meant to express: tokens that would have
been sent *uncached*, or tokens that would have been *cache reads*. Price each
at its own rate. On a workload hitting cache 99% of the time, the honest
figure is likely far below $247.92 lifetime.

Note this is a separate defect from items 1–1c. Those concern `tok_saved` in
PERF lines; this concerns the persisted ledger and the money figure. The two
ledgers do not agree in structure and should be reconciled deliberately —
PERF booked 4,374,269 tokens against the ledger's 3,664,409 over the same run.

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

## 12. Code paths with no instrumentation at all

An audit of `headroom-proxy` and `headroom-core` looking for silent paths
rather than reading metrics. Ranked by risk. None of these are confirmed
faults — they are places where a fault would leave no trace.

1. **`headroom-core/src/ccr/response_handler.rs` — zero tracing calls in
   ~700 lines.** `parse_ccr_tool_calls` (:199), `extract_tool_calls` (:108)
   and `create_tool_result_message` (:244) drive the CCR retrieval round-trip
   shipped in `35b75455`. A hash mismatch, an unresolvable tool-call id
   (`unwrap_or("")` at :214-228) or a retrieval returning empty content
   produces no log line. Newest code, least tested, completely dark.
2. **`headroom-core/src/ccr/context_tracker.rs` — zero tracing calls.** LRU
   eviction at :174-179 silently drops tracked contexts past
   `max_tracked_contexts`. If proactive expansion stops matching because
   entries were evicted early, the only symptom is a slow decline in savings
   numbers that are themselves estimates.
3. **`headroom-proxy/src/ctx/offload_store.rs:120-149` `persist_one`** writes
   to the CCR store and the FTS index independently, then returns `ccr_ok` as
   the overall result. A half-failure is not distinguished anywhere. Since
   `/ctx/get` reads only CCR and `/ctx/search` only the index, a CCR-side
   failure leaves a record that is searchable but not retrievable, with no
   line saying which half failed.
4. **`headroom-proxy/src/semantic_cache.rs`** is invisible except at two call
   sites in `proxy.rs`. No signal on eviction (:80-88) or TTL expiry
   (:148-154). An undersized `max_entries` that evicts on every insert would
   look exactly like a cache that never helps.
5. **`headroom-proxy/src/memory/ctx_backend.rs:211-232`** — `delete_memory`
   and `clear_user` re-index with `let _ = self.index.index_content(...)`.
   An index failure after a successful record delete leaves an orphaned FTS
   entry, swallowed with no log. The orphan-skip path at :172 does log
   `memory_index_orphan`, so the inconsistency is visible in the file itself.
6. **`headroom-core/src/ctx/store.rs:232`** — `purge_all` does
   `conn.execute("DELETE FROM chunks", []).unwrap_or(0)`, turning a SQL error
   into a fake "0 rows deleted", then runs three more deletes with `?` that
   would propagate. Same function, two error policies.
7. **`headroom-core/src/ccr/backends/in_memory.rs:90-98`** — capacity-forced
   eviction of unexpired offloaded content has no signal. Offloaded output
   becomes unretrievable and nothing distinguishes eviction pressure from
   TTL expiry.
8. **`codex_rate_limits.rs:148` `extract_rate_limits`** returns `None`
   silently when the response shape moves outside the
   `["response","info","item"]` checklist. The statusline segment would go
   stale with no warning.
9. **`prefix_replay.rs:317-388`** computes a `changed` bool and discards it
   (`let _ = changed;` at :388); the caller at `proxy.rs:4409` re-derives the
   same thing. Harmless today, dead signal tomorrow.

Checked and found adequately instrumented:
`cache_stabilization/anthropic_cache_control.rs` (logs every skip and apply),
`sse/anthropic.rs` usage merging, `usage_observer.rs`. Both
`cache_hit_rate.rs` and `usage_observer.rs` correctly key off upstream
`usage` rather than proxy estimates, so the item 2 and item 10 estimate
problems do not extend to them.

## 13. CCR store integrity — checked, no fault found

A correctness risk with no metric, so tested directly. The proxy offloaded
this session's own tool output several times, leaving `<<ccr:HASH>>` markers.
Read-only query against `~/.claude-work/context-mode/ccr.db`:

- 538 rows in `ccr_entries`; **10 of 10 markers observed in live output are
  present**.
- `original` is never null or empty; lengths run 521 B to 133 KB, mean 8.9 KB.
  Nothing suspiciously small.
- Uniform TTL of 604,800 s (7 days); **zero rows expired**. Entries span
  2026-08-02 to now.

Limit of this test: a present row with plausible length proves the entry
exists, not that its content is intact or correctly decompressible. A fuller
check retrieves through the real read path and compares against the original.
Given item 12.1 — the retrieval code has no instrumentation at all — that
fuller check is worth doing before trusting this result.

One number here is worth a second look, though it is not a fault: **532 of
538 rows have `last_accessed <= created_at`**, meaning only 6 entries have
ever been read back. Either offloaded content is rarely needed, in which case
the retrieval machinery is costing more than it returns, or retrieval is
failing quietly and nobody would know, which is exactly item 12.1. The two
explanations are indistinguishable from the outside — another argument for
instrumenting the read path.

## 15. The codex/OpenAI route reports savings with no compression behind them

Observed 2026-08-08 22:30-22:39Z while a second agent drove
`claude-codex-5.6` (routed to `gpt-5.6-luna`). The route is otherwise the
healthiest thing in this document: 69 routed, **69 booked, zero unbooked**, zero
negative `tok_after`, median total 2,964ms, ttfb 911ms.

**But `tok_saved` is re-emitted, not measured.** Consecutive turns on one
conversation:

```
22:36:38  msgs=134  tok_before=67,980  cache_read=62,208  tok_saved=4,522
22:37:23  msgs=150  tok_before=70,279  cache_read=65,280  tok_saved=4,522
22:38:35  msgs=153  tok_before=70,433  cache_read=65,280  tok_saved=4,522
```

`tok_before` climbs, `tok_saved` is pinned. Across the hour the route produced
just 7 distinct `tok_saved` values over 74 turns (4522 x30, 519 x24, 2454 x13).

**And none of them has a compression event.** Joining on request id:
`compression applied` lines matching a codex turn: **0 of 74**. The route books
178,006 tokens of savings across the run without a single compression event to
account for them.

This is item 1b's re-emission on a different route, and it isolates that
mechanism from the scope mismatch in 1d — here there is no compression at all,
so a stale per-conversation value is the only possible source. Whatever 1d's
fix does to `tok_before`, this path still reports a saving nothing produced.

**Confirmed on a larger sample (156 turns since 22:30Z).** 14 distinct
`tok_saved` values, mean 11.1 repeats. Each value holds while its conversation
grows by 50+ messages and ~10K tokens:

```
tok_saved= 4522 x30  msgs  83..153  tok_before  62,040..70,433
tok_saved=11541 x27  msgs 201..254  tok_before 100,203..110,679
tok_saved=14651 x21  msgs 294..336  tok_before 120,489..129,190
```

Same shape as 1e on the Anthropic route: pinned saving, moving baseline. Two
routes, two code paths, one mechanism — which argues the stale value originates
in something both share (the offload/CTX layer) rather than in either handler.

**Cache misses here are single-turn and self-healing.** 4 of 156 turns read zero
cache; each recovers on the very next turn (22:55:42 reads 0, 22:55:45 reads
112,384). Not the item 14 pattern, and not worth chasing — but note a
`cache_read==0` alert rule will fire on them, which is what produced the
`COLD-CACHE msgs=325` event that led here.

**Where to look:** `handlers/local_model.rs`, which owns this route (see
`optimized_tokens: input_tokens` at :3082 and the ChatGPT redirect at
:1238-1244), rather than the `proxy.rs` booking path items 1-1d concern.

Two smaller notes on the same route:

- `cache_write` is **exactly 0** across all 69 turns while `cache_read` totals
  3.1M. Reads without a single recorded write. Either the provider reports cache
  usage differently here, or writes are not being read off this response shape —
  worth one check before anyone quotes a cache-hit figure for this route.
- `model_route_translate` (`local_model.rs`) carries **no `request_id`** — only
  model, upstream, stream. The route is still joinable through the *translated*
  model name on the PERF line (`gpt-5.6-luna`, not `claude-codex-5.6`), which is
  the non-obvious step: searching for "codex" in PERF returns nothing and looks
  like an instrumentation gap. Adding `request_id` to that event would remove
  the trap.

## 14. Rate limiting costs cache, not just time

Observed live at 22:19-22:21Z, immediately after the 429 burst in item 7. Over
22:00-22:29Z there were 120 turns on conversations of 40+ messages: **117 warm,
3 cold — and all 3 cold starts fall in the two minutes after the rate limit
cleared.**

```
22:16:46      msgs=149  cache_read= 22,032  cache_write= 76,034
              <- 429 burst, ~2.5 min gap, no traffic ->
22:19:05 COLD msgs= 95   cache_read=      0  cache_write= 95,933
22:19:14      msgs= 97   cache_read= 95,933  cache_write=  1,388
22:19:21 COLD msgs=152   cache_read=      0  cache_write=101,564
22:19:30      msgs=155   cache_read=101,564  cache_write=  1,540
22:21:33 COLD msgs= 65   cache_read=      0  cache_write=128,514
```

Each cold turn rewrites the whole prefix, and the very next turn on that
conversation reads back exactly what it wrote (95,933 → 95,933; 101,564 →
101,564). The cache works — it had simply gone.

**The gap is under the 5-minute TTL.** 22:16:46 to 22:19:05 is 2m19s, so TTL
expiry alone does not explain it. Candidates, in order of likelihood:

1. The retries themselves. Three requests exhausted their attempts against a
   rate limit; if a rejected request still counts as a cache write attempt
   upstream, or if the clamped 30s retry landed while the limit was open, the
   prefix may have been evicted rather than expired.
2. The clamp in item 7 — retrying into a closed window churns the prefix.
3. Ordinary eviction under concurrency, unrelated to the limit.

**Cost.** 326,011 tokens of `cache_write` across the three, against ~1,400 on a
normal warm turn. Being rate-limited is not just a delay; it bills a full
prefix rewrite per affected conversation once traffic resumes.

Worth checking against item 7's fix: if honouring a longer `retry-after`
prevents the wasted retries, it may also prevent this. That would make the
clamp decision worth more than the delay it saves.

## 8. Smaller notes

- Two `injection row missing for a known conversation; injecting nothing
  (fail-safe)` events (10:33:11, 11:49:07). The fail-safe held, but those
  turns silently got no injection and nothing downstream records it. Neither
  log line carries a request_id, so they can't be tied to a turn.
- Startup warns AWS creds absent for Bedrock. Harmless unless routing there.
- Every request logs `non-PAYG auth mode`, skipping cache_control
  auto-placement and both sort passes (328/328 requests). Correct for a
  subscription, but those three optimisations are inert — worth knowing
  before attributing savings to them.
- 68 unparseable lines in the log file overall.
- `tools[]` indices for the same static sample vary across requests (25, 34,
  39, 41, 48, 55). Probably differing MCP sets per session rather than
  reordering within a session, but unconfirmed.

## Healthy — checked, no action

- `opt_ms` median 11ms, p90 29ms, max 211ms. Proxy adds no measurable latency.
- `ttfb_ms` median 1571, p90 3129. The 77.8s max traces to upstream retries.
- `cache_hit_pct` 99 median, 9 cold starts out of 311 turns.
- Cold cache on a large conversation after an idle gap is expected, not a
  fault. The run went quiet 12:00Z–20:00Z; the first turns back (156 and 104
  messages) wrote 155K and 88K tokens with zero cache read. Upstream cache TTL
  is minutes, so nothing survives an eight-hour pause. Same process
  throughout — no restart, so the scoping baseline above still holds.
- `tok_inflated` is 0 across every turn.

---

## Hunches tested and dropped

- **"Injection failure causes the `early_messages` busts."** Prompted by
  11:49:07 (injection missing) sitting 3 seconds before 11:49:10 (22,997
  tokens wasted, `early_messages`). It does not generalise: the other
  injection failure at 10:33:11 has no recache within 120s, and 11 of the 12
  genuine-drift events have no injection failure anywhere near them. Recorded
  so nobody re-runs it. The single pairing may still be real for that one
  turn — it is just not the general cause.

---

## 16. The proxy silently edits the *content* of tool results

Everything above is about the proxy mis-counting its own work. This one is
different in kind: the proxy changed data that an agent then read and acted
on. It should be fixed before any of the metric items.

**The observation.** At 23:04 I read `cache_recache_observed` records
straight out of `headroom-proxy.log` with a Python one-liner, printing the
`timestamp` field verbatim. Two of the five records came back as:

```
2026-08-08T23:2:36.174635Z  ...  wasted_tokens 1965
2026-08-08T23:4:38.372256Z  ...  wasted_tokens 143871
```

The minute field is not zero-padded. Reading the same two records again by
`request_id` gives the true values:

```
2026-08-08T23:02:36.174635Z 1965 162870 f4993f01a4bc27b6
2026-08-08T23:04:38.372256Z 143871 22032 f4993f01a4bc27b6
```

The file is not corrupt. A scan of the whole log — 142,432 JSON records with
a `timestamp` field — finds **zero** malformed against
`^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z$`. The digit was removed between
the file and the conversation, i.e. inside the proxy.

**What survived tells you what the rule is.** In the same payload:

| Fragment | Contains `0N`? | Altered? |
| --- | --- | --- |
| `2026-08-08` (date) | yes, twice | no |
| `.174635` (microseconds) | no | no |
| `22:32:11` | no | no |
| `23:02:36` → `23:2:36` | yes | **yes** |
| `23:04:38` → `23:4:38` | yes | **yes** |
| `f4993f01a4bc27b6` (hex key) | yes (`01`) | no |
| `143871`, `22032` (counts) | no | no |

So it is not a generic leading-zero stripper — the date and the hex key both
contain `0N` groups and both survived. Only a zero-padded group *in a
colon-delimited time* was re-rendered. That signature is a parse-then-format
round trip: something recognises `HH:MM:SS`, parses the minute as an integer,
and writes it back with `to_string()` / `format!("{}")` instead of
`format!("{:02}")`.

**Where to look first.** `crates/headroom-core/src/transforms/pipeline/
reformats/log_template.rs` is the obvious candidate by name — templating log
lines means splitting them into a shape plus extracted variables, and
re-rendering is exactly where padding is lost. `log_compressor.rs` and
`json_minifier.rs` are the next two. Unconfirmed at the time of writing; a
search was running when this was filed. Whoever picks this up should confirm
the site before changing anything, because the fix is one format specifier
and the risk is fixing the wrong one.

**Why this outranks the metric bugs.** A wrong `tok_saved` misleads whoever
reads the dashboard. A wrong digit inside a tool result is fed to a model as
fact. Timestamps are the visible case because they are checkable against the
source file; the same round trip would silently damage version numbers
(`1.09` → `1.9`), zero-padded IDs, ports, exit codes, hashes rendered with
colons — anything where a padded number carries meaning. Nothing in the
current instrumentation would catch it: the proxy books this as a saving.

**Reproduce it.** Print any text through the proxy containing a zero-padded
time and compare with the source:

```bash
grep -o '"timestamp":"[^"]*"' ~/headroom-proxy.log | grep -E '23:0[0-9]:' | head
```

then read the same lines back through a tool result and diff. If the minute
survives, the responsible transform did not fire for that payload — vary the
size, since compression is size-gated.

**Related.** Worth re-checking the "the report came through truncated"
messages seen during the session against this. They were read at the time as
the model's own summarising, and no evidence links them yet — but a transform
that edits content makes the literal reading worth a second look.

---

## How to reproduce every figure here

`scripts/proxy_log_audit.py` re-derives them from a proxy log. It prints
derived counts only, never raw log lines.

```
python scripts/proxy_log_audit.py all --log ~/headroom-proxy.log \
    --since 2026-08-08T10:33:08
```

| subcommand | item |
| --- | --- |
| `negtokens` | 1, 1a |
| `cumulative` | 1c |
| `recache` | 3, 3a, 11 |
| `coldstart` | 3c |
| `volatile` | 4 |
| `retries` | 6, 7 |
| `unbooked` | 9 |
| `ledger` | 10 |
| `ccr` | 13 |

**`--since` is not optional in practice.** The log is never rotated, so
without it every count spans months and many restarts. Use the process start
timestamp. Figures in this document use `2026-08-08T10:33:08`.

Numbers will not match this document exactly if you run against a longer
window — the run continued after the document was written. What should hold
is the *shape*: the ratios between transforms in item 1a, and the all-upward
disagreement in item 1c. If one of those flips, the finding is wrong and should
be struck.

`retry_after seen anywhere: 0` is **not** a shape-invariant — an earlier
revision listed it as one. It reports a field the proxy never logs, so it reads
0 whatever the retry code does. It will keep reading 0 after item 7 is fixed in
behaviour, and only change when the log field is added.

Live watcher used during the observation: `/tmp/hr_watch.py`, a throwaway
that tails the log and prints errors, exhausted retries, recache waste over
5K tokens, negative token counts and cold cache on large conversations. Not
preserved; the audit script covers the same ground after the fact.

## 18 — Anthropic rejects one turn in seven, and the proxy never saw why

Found 2026-08-09 while chasing item 9. Requests with no completion record are
requests the provider refused: a 400 body is not an event stream, so the SSE
parser never runs, `store.complete()` never fires, and the replay prefix for
that stream freezes where it was.

Rate, by day, over forwarded Anthropic requests:

| day | forwarded | 400 |
| --- | --- | --- |
| 2026-07-27 | 1887 | 0 |
| 2026-07-28 | 1510 | 0 |
| 2026-08-07 | 2822 | 62 |
| 2026-08-08 | 3347 | **469 (14%)** |
| 2026-08-09 (to 15:30Z) | 1167 | 185 |

### Why it was invisible

`should_buffer_for_cache = !is_sse && status.is_success()` — an error body
streams straight through to the client, unread. The log carried
`upstream_status=400` and nothing else. Fixed: `upstream_rejected` (warn) now
buffers the body and logs the provider's `error.type` and a 400-character
`error.message`. An unrecognised envelope logs empty strings, never raw bytes,
so the log is not a place unknown payloads land.

### The reason, and who caused it

> `messages.1.content.17`: `thinking` or `redacted_thinking` blocks in the
> latest assistant message cannot be modified.

Attribution needed a second event, because both the client and this proxy
rewrite history. `messages_rewritten` reports the indices the proxy altered,
and separately the indices where a signed reasoning block itself differs on the
wire — compared raw, since `cache_control` is the one key the proxy rewrites
every turn by design and the canonical compare is blind to exactly that.

On every rejected turn the proxy rewrote messages `[0,10]` and touched no
signed block. Anthropic names message 1.

The pair that settles it — same turn, one second apart, identical proxy
rewrites, opposite outcomes:

```
15:27:37 msgs=120 st=400 rw=[0,10] signed=[] first_diff=1
15:27:38 msgs=120 st=200 rw=[0,10] signed=[] replay applied
```

The client sends a thinking block Anthropic refuses, takes the 400, retries
without it, and succeeds. The proxy does the same thing to both attempts.

### What it costs

One wasted round-trip per affected turn, about a second, and no tokens: across
790 rejected requests in the whole log, **zero** `turn_cost_ledger`,
`savings_placement`, `savings_pricing_counterfactual` or `PERF` events were
booked. The ledger never saw them, so no false savings and no false busts.

It also explains the standing `first_diff_index=1` decline. The store keeps the
successful, thinking-free version of message 1, so the next turn's first
attempt always diverges there. That decline is free — the attempt it belongs to
is rejected and never billed.

### What to watch

`thinking_touched_indices` must stay empty. A non-empty list means the proxy is
modifying a signed block, which the provider refuses outright — a defect
regardless of what it saves.

## 19 — Partial prefix replay: built, measured, reverted

Tried and rejected 2026-08-09. Recorded because the reason overturns an
assumption several other items rest on.

The idea was obvious enough: `overlay_cached_prefix_reported` refuses the whole
stored prefix on any mismatch, so replay `prev_fwd[..k] ++ optimized[k..]`
where `k` is the first disagreeing message, and stop there. Implemented that
way, with the divergence as a hard cap.

Its first live firing, on one conversation, consecutive turns:

| time | msgs | outcome | cache_read | cache_write |
| --- | --- | --- | --- | --- |
| 16:03:17 | 308 | full replay | 224,689 | 981 |
| **16:03:32** | **313** | **partial, k=241** | **22,026** | **204,768** |
| 16:03:45 | 316 | full replay | 226,794 | 1,855 |

The gap was 15 seconds, so the 5-minute TTL is not the explanation.

### The assumption that was wrong

A declined replay is not a bust. Compression is deterministic, so a turn's own
freshly compressed bytes for an unchanged prefix reproduce the bytes the
provider already holds. The same conversation shows it directly:

| 16:02:41 | 302 | `no_previous_turn` — replayed nothing | 222,975 | 342 |

A turn that replayed nothing still read 222,975 tokens from cache. Replay earns
its keep by holding bytes stable when compression *would* have drifted, not by
rescuing a prefix that decline throws away.

Splicing one turn's bytes onto another turn's tail makes a seam that matches
neither, which is the one reliable way to actually lose the prefix.

### What this costs elsewhere

Item 17 says turns that skip replay were 19% of traffic and carried 97% of
booked re-cache waste. That is a correlation, and the causal reading now looks
backwards: a client edit mid-history causes both the divergence and a genuine
bust, so the skip flag marks the turns rather than causing their cost. Any
figure derived from the skip flag needs re-deriving against `cache_read`.

The 131,469-token turn that motivated this item needs the same treatment before
it is quoted again.

### Left behind

The all-or-nothing path carries a comment with these numbers, and
`overlay_declines_whole_prefix_when_a_later_message_diverges` pins the
behaviour, so the next reader who spots the "obvious" improvement finds the
measurement first.

## 20 — Where the diverged busts actually come from

Measured 2026-08-09 after item 19's revert, over 164 `prefix_content_diverged`
turns since 15:00Z.

**The divergence is early, not at the boundary.** 122 of 164 land within the
first five messages; only 35 are within five of the end. The "client appended a
block to the newest message" picture that shaped items 7 and 17 is the minority
case.

Shapes, stored original → current original (both are client bytes):

| transition | n |
| --- | --- |
| `[text,tool_use]` → `[thinking,text,tool_use]` | 78 |
| `[tool_result,tool_result,text,text]` → `[tool_result,tool_result]` | 21 |
| `[tool_use]` → `[thinking,thinking,tool_use]` | 18 |
| `[tool_result,text]` → `[tool_result]` | 16 |
| `[tool_result×3,text×3]` → `[tool_result×3]` | 7 |

Two client behaviours, neither of them the proxy's doing:

- **96 thinking-block additions.** These are item 18's rejected first attempts.
  The client sends thinking blocks, Anthropic refuses, it retries without them.
  The store keeps the successful shape, so the next first attempt diverges. Free
  — those attempts are never billed.
- **47 with one `text` block per `tool_result`**, present in the stored original
  and absent from the current one. The one-to-one structure says these are
  ephemeral notes the client attaches to arriving tool results and drops on the
  following turn — `<system-reminder>` blocks fit exactly. Inferred from block
  structure; not confirmed against the client.

The second class costs real tokens. 34 diverged turns took a large
`cache_creation` write with the provider cache still warm (gap under 300s),
concentrated in small conversations of 14–36 messages writing 50–86K each. The
client's own bytes move at message ~3, so the provider's prefix breaks there
whatever the proxy does.

**The proxy cannot fix this without lying to the model.** Holding the previous
turn's version stable would keep the cache, and would mean re-sending a note the
client deliberately withdrew — the same objection that caps replay at the first
divergence in item 19. That is a behaviour trade, not an optimisation, and it
needs an owner's decision rather than a patch.

Not attempted. Recorded so the next reader does not re-derive it.

## 21 — Replay hits 95% of the time; the residual cannot be attributed yet

Measured 2026-08-09, 16:00–16:55Z, grouping by `conversation_key` from
`turn_cost_ledger` rather than by message count.

| full replays | n |
| --- | --- |
| clean hit — under 5K written, over 50K read | 328 |
| bust — 60K or more written | 16 |

The 16:

- 5 followed an idle gap past the provider's 5-minute TTL (up to 1472s). The
  bytes were gone whatever the proxy held.
- 1 was a genuine first turn on its key.
- 10 remain unexplained.

**The multi-stream store is not the cause.** `prefix_replay_matched_alternate`
fired 6 times in the window and every one of them landed among the 328 clean
hits; none of the 16 busts matched an alternate. Replaying another stream's
prefix was the obvious suspect and the data clears it.

**Why the last 10 cannot be settled.** Every one reads exactly the
system-and-tools block (22,026 / 22,277 / 31,028 / 31,086 recur across
different conversations) and rewrites every message. Their apparent inter-turn
gaps are 0–145s, which would make them warm — but `conversation_key` is
`SHA256(session_key + messages[0])`, so subagents forked from one parent share
a key. Item 11 fixed the replay *store* to hold several prefixes; it did not
change the key. Gaps of 0 and 1 second are therefore probably different streams,
not one stream taking two turns.

Candidates, in the order worth testing: streams merged by the key; provider
cache-write visibility under the concurrent subagent bursts these all sit in
(item 3b); something real in the proxy. Distinguishing them needs a per-stream
identifier on the turn events, which nothing currently emits.

Not chased further — the instrument does not exist yet, and guessing between
three candidates is how item 19 went wrong.

## 22 — What the proxy actually costs and saves, measured

2026-08-09, 1247 booked turns, all of them `all_messages`. There is no period
in the log with compression off, so this is not the A/B — but it answers most of
the question without one.

### Compression is not where the money is

Client sent 485.8 MB, the proxy forwarded 470.6 MB: **3.1% removed**.

| billed input | tokens | share | rate |
| --- | --- | --- | --- |
| fresh input | 55,096 | 0.0% | 1.00x |
| cache read | 128,907,409 | 91.1% | 0.10x |
| cache write | 12,575,706 | 8.9% | 1.25x |

91% of input is served from cache, so 91% of anything compression removes would
have been billed at a tenth. Upper bound on compression's contribution is
roughly 1.5% of the bill, and that ignores any cache write it causes by moving
bytes.

### Cache writes are 55% of the bill

12.6M write tokens at 1.25x is 15.7M fresh-equivalents out of a 28.7M total,
from 8.9% of the tokens.

### 38% of the bill is re-caching what was already cached

174 re-cache events today, 8,733,092 wasted tokens — 69.4% of all cache-write
volume, 10.9M fresh-equivalents, **38.1% of the input bill**. `wasted_tokens` is
`min(expected_read - actual_read, cache_creation)`, so it is an upper bound: it
assumes the prefix should have been readable.

| replay outcome | events | wasted |
| --- | --- | --- |
| `prefix_content_diverged` | 107 | 6,339,455 |
| replay applied | 65 | 2,337,405 |
| other | 2 | 56,232 |

Breaking the diverged bucket down by what actually changed. Restricted to the
106 events carrying the full divergence diagnostics — the proxy was rebuilt
several times today and earlier events lack those fields, so including them
invents a phantom class:

| class | events | wasted | share of diverged | median idx |
| --- | --- | --- | --- | --- |
| client **dropped** text block(s) | 95 | 4,353,443 | 78.6% | 3 |
| client added block(s) | 8 | 1,098,858 | 19.8% | 171 |
| thinking blocks | 2 | 46,384 | 0.8% | 4 |

So the price of the behaviour trade item 20 left open is **19.0% of the input
bill** — 4.35M wasted tokens, 5.44M fresh-equivalents. An earlier pass here said
27.6%, reached by attributing the whole diverged bucket to one class.

Item 18's thinking-block divergences are confirmed free: 2 events and 46K
tokens, because those attempts are rejected and never billed.

On these turns `expected_cache_read` totals 6,690,953 against an actual
3,092,273, so the prefix still half-hits — the break is real but not total.

### Reading of the original question

"Is the proxy saving tokens?" Compression: almost nothing, ~1.5% at best. Cache
stabilisation: that is the whole product, and 38% of the bill is still on the
table. Effort spent on compression ratios is effort spent on the 3%.

### Scoping trap, twice

`proxy.log` is never rotated. The first pass at this summed `cache_recache_observed`
across months against one day of `turn_cost_ledger` and produced waste at 123.6%
of the bill. Any figure here must be scoped by date.

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

## 25 — Premises that failed on 2026-08-09, and what killed each

Kept because each one looked well-supported when acted on, and the thing that
disproved it was cheap and available beforehand. All three share a shape:
**a mechanism was confirmed to exist, then a large number was attributed to it
without checking what else produces the same signature.**

### 1. "Replaying the leading run that still agrees recovers busted prefixes"

*Predicted:* a divergence at message k throws away k messages of cached prefix,
so replaying `prev_fwd[..k]` recovers them.

*Killed by:* the same conversation's own turns. A `no_previous_turn` turn, which
replayed nothing at all, still read 222,975 tokens from cache. Compression is
deterministic, so a turn's own bytes for an unchanged prefix already reproduce
what the provider holds — a decline was never the loss. The partial splice
matched neither turn's bytes and cost 204,768 tokens on its first firing.

*Available beforehand:* `no_previous_turn` turns with full cache reads were
already in the log. The skip reason was read as a cost without ever checking
what those turns billed.

### 2. "`<system-reminder>` churn costs 19% of the input bill"

*Predicted:* the client withdraws a reminder from an early `tool_result`
message, breaking the prefix there.

*Killed by:* splitting the same waste by whether the conversation key carries
more than one advancing sequence. 60% of it sat on multi-sequence keys, where a
divergence at message 8 means "one sequence has a reminder there, the other does
not" rather than "the client withdrew it". The defensible figure is **1.5%**.

*Available beforehand:* message counts running backwards is item 11's documented
fingerprint and had been in this document all day.

### 3. "`conversation_key` merges concurrent sessions"

*Predicted:* three Claude Code sessions on one machine share auth and IP, so
`derive_session_key` collapses them and `conversation_key` is left to separate
everything on `system + messages[0]`.

*Killed by:* counting them. **68 distinct `session_key_hash` values in one day,
each mapping to exactly one conversation key.** Sessions and subagents are
already separated; nothing is collapsing.

*Available beforehand:* one `group by` over a field already on every
`cache_recache_observed` event.

### What survives

- Replay hits 328 of 344 full replays cleanly.
- Cache writes are 55% of the input bill; compression removes 3.1% of wire bytes
  and is worth ~1.5% at most.
- 84% of re-cache waste sits on keys whose turns need more than one advancing
  sequence. **The observation survives; the cause does not.** Compaction,
  branching and concurrency all produce that shape and have not been separated.

### The measurement discipline this earns

Before attributing tokens to a cause, list every other process that produces the
same signature and rule them out by query. Message counts, skip reasons and
block shapes are all consistent with several causes at once. And scope by date:
`proxy.log` is never rotated, and the proxy was rebuilt eight times today, so
"today" mixes log schemas as well as months of history.

## 26 — Chain id: a grouping key that is not a guess

Added 2026-08-09, live in `e22c1152`.

Item 25's three failures all turned on the same missing thing: no way to tell
which turns belong to one unbroken run. `session_key` is per-client,
`conversation_key` hashes `system` plus the first message, and message counts
are ambiguous — a compaction, a retry and a genuine second stream all make the
count stop rising.

The replay store already computes the answer, because it has to decide what to
replay. A *chain* is a run of turns that each continue the previous one. Each
stored prefix now carries an id, assigned when a chain is born and inherited by
every turn that continues it. `previous_turn_for` returns it and both
`prefix_replay_applied` and `prefix_replay_not_replayed` log it as `chain_id`.

`chain_id = 0` means this turn continues nothing held — a first turn, a TTL
eviction, or a branch. It is deliberately not the id of the prefix the fallback
hands back, because naming two unrelated runs the same thing is the bug this
exists to remove.

Pinned by `interleaved_streams_get_distinct_chain_ids` (two streams get two ids,
each surviving growth) and `a_turn_continuing_nothing_reports_no_chain`.

### What it can now answer

- Of the 84% of re-cache waste sitting on multi-sequence keys, how much is
  concurrent streams and how much is one stream compacting or branching. Two
  live chains on one key means concurrency; a chain ending and a new one
  starting on the same key means a branch or compaction.
- Whether the gaps used to rule out TTL expiry (items 21, 22) were measured
  between turns of the same run, or across two runs — the earlier figures
  grouped by `conversation_key` and cannot tell.

Nothing consumes it yet. It is a measurement first; migrating the drift
detector, savings attribution or ctx-inject onto it is a separate decision and
should wait until the data says the id behaves.

## 27 — Item 21's hit rate was measured on mislabelled turns

Found 2026-08-09 while checking why chain ids reported "replayed /
no_tracker_for_session", which is impossible.

**The trap.** Both `prefix_replay_not_replayed` and `prefix_replay_applied` fire
for the same request. The second does not mean the replay succeeded — it means
the forwarded bytes changed, which includes a turn that declined the replay and
only had its `cache_control` renormalised. 148 turns in a 90-minute window log
both. Any query that reads `prefix_replay_applied` as "replay succeeded" counts
those as successes.

Item 21 did. Re-derived, with a real replay defined as *no decline logged*:

| | turns | clean hit | bust >=60K |
| --- | --- | --- | --- |
| true full replays | 1266 | 1173 | 7 (0.6%) |
| declined turns | 166 | 24 | 53 (32%) |

Full replay is far better than item 21 said — 0.6% bust, not 16 in 344.

**And declines are far worse than item 19 said.** That item concluded "a declined
replay is not a bust", from one conversation where a `no_previous_turn` turn
still read 222,975 tokens from cache. Across 166 declines, 53 took a large write.
The revert in item 19 still stands on its own measurement — splicing one turn's
bytes onto another's tail cost 204,768 tokens on its first firing — but the
reasoning attached to it was drawn from a single favourable sample and is too
strong. A decline is *often* a bust; what does not follow is that a partial
splice fixes it.

Caveat on the table: "clean hit" requires over 50K read, which a small
conversation cannot reach, so the hit column is biased toward long
conversations. The bust rates are not affected by that bias in the same way, and
0.6% against 32% is not a threshold artefact.

**Rule this earns.** `prefix_replay_applied` means "bytes changed". The only
sound test for a replay is the absence of `prefix_replay_not_replayed` on the
same request id.

## 28 — The churn disguises itself as a branch, and that is the way in

Live in `b4f97810`, 2026-08-09.

### The thing that was hiding

Item 27 left declines as the target: 32% of them take a large write, against
0.6% of true replays. Item 19 already proved the obvious fix wrong — a decline
happens because the client changed message *k*, so the provider's prefix dies at
*k* whatever we do, and replaying `prev_fwd[..k]` only preserves a region that
was already fine while adding a seam.

The way in came from the chain ids. Every large-write decline in the sample
reported `chain_id = 0` — continuing nothing — and three of four involved a
`<system-reminder>`. But chain identity is decided by the *same* canonicalizer
the reminder churn defeats. A turn that merely lost a reminder is judged to
continue nothing at all. The churn was wearing a branch's clothes, in the one
signal built to tell them apart.

### Two halves, and they only work together

**Compare blind to them.** `canonicalize_for_prefix_compare` now drops
`<system-reminder>` text blocks, exactly as it drops `cache_control`. A turn
that withdrew one still continues its chain, so replay engages.

**Forward without them.** `relocate_ephemeral_blocks` lifts every reminder out
of history and re-attaches it to the newest message. Nothing is dropped; the
model receives every block the client sent, in the same request, moved to where
the breakpoint leaves it outside the cached prefix.

Half one alone would freeze each turn's reminder into the replayed prefix
forever — the accumulation item 20 worried about. Half two alone leaves the
comparison failing, so the replay never engages to begin with. Ignoring a
difference while still forwarding it would be worse than either: replaying bytes
the provider never cached.

### The invariant that decides whether it works

On turn N a reminder rides on the newest message; on turn N+1 that message is
history and is stripped, so its bytes change. That would kill the cache if the
breakpoint sat inside the changed part. It does not — the marker goes on the
last non-ephemeral block (item 23's seal), so everything actually cached is
byte-identical across the boundary. Pinned by
`the_cached_region_survives_the_newest_message_becoming_history`.

Also pinned: history is identical whether or not a reminder was sent; no block
is lost in the move; a message is never stripped empty (the API rejects that);
nothing moves when the newest message is an assistant prefill or carries string
content; a real edit beside a reminder is still seen.

3739 tests pass, clippy clean.

### What would show it working, and what would show it wrong

Working: reminder-involved declines fall toward zero, and the `chain_id = 0`
rate on large-write declines falls with them.

Wrong: a rise in 400s (`upstream_rejected`), which would mean the relocation
produces bodies the API refuses — the risk the empty-content and prefill guards
exist to prevent.

## 29 — Message-level relocation, moved to the front of the pipeline

Live in `c44bf160`, 2026-08-09.

Item 28 shipped block-level relocation and it changed nothing measurable: 252
turns, decline rate 13% against 11.6% before, 30% of declines still taking a
large write. The check found why. Of 18 surviving divergences, 12 were one
shape:

```
stored [text] -> current [string]     kinds: [system-reminder] -> []
first_diff_path: content[len 0 vs 1]
```

A message whose entire content was a `<system-reminder>` had vanished, shifting
every index after it. Item 28 walked straight past that case, by a guard written
to avoid a 400:

```rust
if keep.is_empty() { continue; }
```

Block-level relocation cannot help when the churn is a whole message.

### Two changes

**A scaffolding-only message is dropped, not emptied**, and its blocks travel to
the newest message with the rest. Safe because the client's own next request is
that same sequence without it — if the shape were invalid, the client's request
would be refused, and it is not.

**Relocation moved to the front of the request path**, ahead of the capture that
records the turn's originals. Every stage below — capture, compression, the
append-only guard, the bytes on the wire — now sees one canonical history that
does not depend on whether a reminder was attached this turn.

That ordering is what makes dropping a message legal. On the forwarded side
alone it would break `forwarded.len() == original.len()`, and
`ForwardedCountMismatch` would decline every turn.

### Why this is not item 8's defect repeated

Item 8 was capturing originals *after* `ctx_offload` had rewritten them. Offload
decisions depend on store state, so the comparison baseline drifted turn to
turn. Relocation is a pure function of the body: same input, same output,
nothing mutable involved. It produces identical history whether or not the
scaffolding was sent, which is the property the whole fix rests on.

A body with no scaffolding in its history is returned byte-identical, so the
transform cannot become a source of churn itself.

3740 tests pass, clippy clean.

### What would show it working, and what would show it wrong

Working: the `[text] -> [string]` divergence shape disappears, and the decline
rate falls below the 11.6% baseline.

Wrong: any non-429 `upstream_rejected`. Dropping a message is the riskiest thing
the proxy now does to a request, and a 400 is how that would surface.
