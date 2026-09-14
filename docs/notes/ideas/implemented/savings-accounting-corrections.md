# Implemented: savings accounting corrections (Aug experiments)

- **Status:** all closed 2026-08-12 with regression tests + live re-measurement
- **Source:** `docs/notes/proxy-experiments-closures.md` (items 1, 2, 10, 15)
- **Summary:** one problem in four faces — cumulative `tok_saved` re-booked
  against a post-CTX baseline (negatives to −46k), scope-mismatched operands,
  fresh-rate dollar pricing (1.3× overstatement → placement-based pricing on
  both durable paths), routed accumulator re-emitting CTX savings. Now:
  `tok_after == tok_before − tok_saved` exact on every turn, ledger rows join
  1:1, headline defined as transform efficiency on selected tokens
  (`selected tokens` label), net verdicts in `/stats.savings_verdict` /
  `wire_verdict`. Do not quote pre-08-12 ratios (item 3's "spent more than
  saved" reversed to 87× net positive after the stream-matching fix).


## Telemetry settlement

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Fix 1d

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Fix 1e/15

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Fix 10

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **10:** savings outcomes now emit fresh-input and cache-read pricing
  counterfactuals with model, token counts, and both dollar values; the durable
  ledger's existing price is unchanged pending a product decision.


## Dropped hypotheses

*moved from `docs/notes/proxy-experiments-2026-08.md`*

Items 3d and 3e record hypotheses that were tested and **dropped** — read them
for what was ruled out, not as open work. Item 3b is a one-line observation, now
superseded by item 11.

---


## Items 1–1e

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Item 2

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Item 3

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Item 3a

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Item 3c

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Item 3d

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Item 3e

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Item 3b

*moved from `docs/notes/proxy-experiments-2026-08.md`*

### 3b. Drift clusters under concurrency

10 of 12 genuine-drift events are `early_messages`, and 8 of those land
between 11:39 and 11:42 — the same three minutes where throughput peaked
(ok turns per minute: 23, 32, 24, 16, against a run median near 5).

Suspicion: parallel requests on one conversation interleave and each sees the
other's prefix. Same suspected mechanism as item 5.


## Item 10

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Item 15

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Closures 1/2/10/15

*moved from `docs/notes/proxy-experiments-closures.md`*

### The savings accounting, in this order

**1 — closed 2026-08-12: `tok_after` no longer goes negative.** Scoped from the
running process's start at 12:52:48Z through 14:07:02Z, 383 JSON-parsed PERF
records contain zero negative or zero `tok_after`, zero `tok_saved >
tok_before`, and zero failures of `tok_after == tok_before - tok_saved`. The
minimum `tok_after` is 2. The maximum saving is 78.96% of its baseline; zero
turns reach 95%, so neither an equality placeholder nor a near-baseline clamp
is hiding the old tail. Aggregate arithmetic is also exact:
`1,909,062 - 934,617 = 974,445`.

The attribution is direct, not inferred. Of those records, 335 join by
`request_id` to a structured `compression applied` event; all 335 before,
after and freed triplets agree exactly. Therefore the former
`tok_saved - tokens_freed` excess is zero on every joined turn, rather than a
conversation-sized value repeating across turns. The current code constructs
`OutcomeContext` from the compression dispatcher's own `tokens_before` and
`tokens_after` and books `tokens_saved: compress_tokens_saved`; CTX offload has
separate accounting and is no longer folded into this subtraction. The live
22:40 regression shape remains pinned by
`sizes_books_only_the_compression_turn_so_tok_after_stays_non_negative`. No
code change was needed for this item.

**2 — closed 2026-08-12: the savings operands now share a scope and turn.** In
the current process window, the 335 positive-saving PERF records total
`tok_before=1,314,871`, `tok_after=380,254`, `tok_saved=934,617`. The durable
ledger has exactly 335 rows for the same PID/window and the same three totals;
zero rows fail `before - after == saved`. Including 48 zero-saving PERF turns
changes the comparison to `1,909,062 - 934,617 = 974,445`, still exact. The
former ~100x discrepancy does not reproduce.

The headline definition is now explicit: `headroom savings` reports transform
efficiency on successful compression events. Its denominator is the
pre-compression input selected by those transforms; it is not the whole prompt
or all provider input, and zero-saving turns are absent from this append-only
ledger. The CLI now says `selected tokens`, and the README and measurement guide
state the scope. `/stats.savings_verdict` remains the net proxy calculation
(compression less cache busts); `/stats.wire_verdict` is the whole-request view
paired with provider-reported usage. No accounting-code change was needed.

**10 — closed 2026-08-12: saved-token dollars now use cache placement.** The
current process confirms the defect but not the original 10x magnitude. Joining
335 `savings_placement` and `savings_pricing_counterfactual` events prices
934,617 saved tokens at $14.019255 when every token is called fresh input,
versus $10.776339 using each turn's cache placement: a 1.301x overstatement.
Dropping each conversation's first turn leaves 333 events, 925,603 tokens and
$13.884045 versus $10.641129, or 1.305x. Of those saved tokens, 694,401 classify
past the cache boundary and 240,216 inside the cache-read prefix. The split is
an upper bound on the valuable share: a selected span that fits in the fresh
tail is wholly called fresh, even if an earlier block contributed.

Both durable dollar paths now use that request-scoped placement. The common
`RequestOutcome` selects `fresh_input` or `cache_read` from its forwarded
selected span and provider usage, resolves the matching rate from the pricing
table, and supplies the result to `SavingsTracker` and the append-only savings
ledger. New ledger events carry `cost_basis`, and the structured pricing event
carries both `priced_cost_basis` and `priced_cost_usd`. Token counts and their
denominators are unchanged. Tests pin both placement branches, the tracker
override, and exact ledger serialization. `cargo check -p headroom-proxy`
passes.

The release build was installed and restarted at 14:26:29Z. Through 14:30:44Z,
all 30 saving turns joined one-to-one across `savings_placement`,
`savings_pricing_counterfactual`, and PID 57588's durable ledger rows. Eighteen
were priced as `fresh_input` and 12 as `cache_read`; zero basis/rate joins
disagreed, zero ledger rows failed `before - after == saved`, and the ledger's
banker's-rounded aggregate exactly matched the structured events at $1.008354.
Calling all 109,503 saved tokens fresh would have reported $1.642545, a 1.629x
overstatement. After dropping each of three conversations' first observed turn,
the comparison is $1.444740 versus $0.810551 across 27 events, or 1.782x. The
CLI and documentation now label dollars as estimates and warn that legacy rows
without `cost_basis` retain their former fresh-input assumption; those rows
cannot be honestly repriced because they never recorded placement.

**15 — closed 2026-08-12: routed savings are turn-local and measured.** The old
sample had 156 routed turns, only 14 distinct repeated `tok_saved` values, and
178,006 booked saved tokens despite zero joined `compression applied` events.
The routed handler used one accumulator for CTX offload and live-zone
compression, then booked that aggregate. It now replaces the CTX count with the
current dispatcher's measured count before booking; tool-schema compaction may
then add its own directly measured saving. CTX remains visible as the separate
`ctx_transform_tokens_saved` field on `routed_compression_accounting`.

The current-day live sample is small but fully joinable. The sole routed turn
at 07:56:47Z carried request ID `b8baddf3-577e-4a77-bbf1-09df6b021087` from
`model_route_translate` through accounting and PERF. Live-zone compression and
CTX each reported zero. Tool-schema compaction was the only shrinking transform,
and PERF booked exactly its six-token result: `770 - 764 = 6`, with
`tool_schema_compaction` named in `transforms`. Thus zero of one current routed
turn has an unexplained saving, versus all 178,006 tokens being unexplained in
the original window. `routed_booking_does_not_reemit_ctx_savings_without_compression`
pins the old 4,522-token repeat shape and proves it now books zero;
`routed_compression_actually_shrinks_a_compressible_body` proves the positive
branch runs the real dispatcher and makes the body smaller. Both tests pass.

PERF intentionally retains the upstream model (`gpt-5.6-sol` in this turn),
because pricing must resolve the model that billed the tokens. The route event
now carries both the client-facing alias (`claude-codex-5.6-sol`) and the same
request ID, so the two names join directly and searching only PERF for “codex”
is no longer the required discovery path.


## Closure 3

*moved from `docs/notes/proxy-experiments-closures.md`*

**3 — closed 2026-08-12: the post-item-11 result reverses the old conclusion.**
The superseded window claimed 505,052 drift-waste tokens against 264,247 saved.
On the completed 12:52:48Z–14:26:29Z process window, 412 booked turns instead
saved 1,077,450 tokens and produced two genuine-drift re-cache events totalling
13,926 tokens. Both are unique request IDs on different conversation keys, 26
minutes apart; the duplicate/concurrent-pair signature from item 3a does not
recur. Four `event_kind=expected` resets total 26,322 tokens and are correctly
excluded from waste.

The next process, through the same 14:35:45Z completed-turn cutoff used for
item 9, adds 56 booked turns and 141,908 saved tokens with zero drift-kind
re-cache events. Its three expected resets total 281,386 tokens; including
those would reproduce exactly the measurement trap this brief warns against.
Across both classifiable windows the like-for-like result is therefore
1,219,358 tokens saved, 13,926 lost to drift, and 1,205,432 net saved. Drift is
1.142% of the saving, or savings outweigh measured drift waste by 87.56x. The
old “spent more than it saved” conclusion does not survive the stream-matching
fix and corrected per-turn savings accounting. No code change was needed.
