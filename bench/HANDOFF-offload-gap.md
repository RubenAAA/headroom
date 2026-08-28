# Handoff: the offload gap

Written 2026-08-18, rewritten 2026-08-19 after a session that landed four
commits. State of the cost-optimisation work on `local/npu-integrated`.

## Where we stand

The proxy costs **more** than plain Claude Code on the older corpus. Over 7,839
captured turns priced under API weights, the client bills 237.0M and the proxy
bills 241.2M — 1.8% the wrong way. Under the corrected subscription weights the
same corpus goes the other way by 1.3% (219.7M against 216.8M), so on blindguard
the proxy is roughly a wash and which side it lands depends on what you are
paying with. On the newer corpus it is 15.9% ahead under API weights and 15.1%
ahead under subscription. That second figure read 27.7% before the weights were
fixed. The two corpora are different builds, not different luck; see **Corpora**
below.

Four commits landed the session before this one and **none of them are running
yet**. The installed binary at `~/.local/bin/headroom-proxy` is still from
2026-08-18T20:20, hours before the first of them, and restarting the process
does not change that — it re-runs the same file. `target/release/headroom-proxy`
now carries all four plus the mid-stream retry below, but nothing has been
installed or restarted. Do that before reading any live counter as evidence
about them.

The offload counters no longer need a live process to check: `offload_replay`
runs the real transform over a corpus. See **Still open**.

```
8dc23eb9  fix(retry): give in-band overload its own, longer budget
4f223054  perf(ctx): stop --exclude-tools from gating offload
e796ee3c  perf(cache): put the message breakpoint on the last content block
95df41d0  fix(ccr): answer every retrieval the model asks for
```

## Which weights to price under

Both. They disagree and the disagreement matters.

API weights: cache read 0.1 of a fresh token, 5m write 1.25, 1h write 2.0.

Subscription weights were fitted from live traffic rather than assumed.
`fit_weights.py fit --window 5h` over 17,152 turns in five log files, 2,783
samples across 28.2h, 1,569 intervals carrying turns, R² 0.328: **cache read
0.10** with a bootstrap band of 0.04–0.25 that excludes zero, and 1h write 1.45
with a band of 1.00–2.00. 5m write is pinned at 1.25 to free the other two, so
it carries the API value by assumption, not evidence.

The fit resolves the 5h window and only the 5h window. Over 7d utilization
moves in 46 intervals against 361 for 5h, and the read band widens to 0.00–0.30,
which does include zero. Any 7d-specific claim is unfitted.

`cachesim.py` now carries those numbers as `SUBSCRIPTION`, and every
subscription figure below has been re-priced under them. **The old `read=0.0`
overstated the marginal value of anything that trades writes for reads, by
roughly 3x on the exclusion arm and 4x on the tail breakpoint.** It did not
change which arms win — it changed how much they win by, and it flattered the
proxy's headline. Under the corrected weights the subscription and API numbers
largely converge, which is what a fitted read of 0.10 against the API's 0.1
should do.

The `fit_weights.py` fix that made this possible: `load_turns` read only
`~/headroom-proxy.log`, which had rotated, so samples and turns had zero time
overlap and every predictor came back R² −inf. `log_paths()` now globs the
rotations, and `fit()` bails with both time spans printed when no interval
carries a turn.

## Corpora

| corpus | turns | window | build |
| --- | --- | --- | --- |
| `~/headroom-capture-blindguard` | 7,839 | 2026-08-16T21:22Z – 08-18T01:11Z | old |
| `~/headroom-capture-windowgap` | 1,150 | 2026-08-18T16:20Z onward | current |

Capture is armed again, into `~/headroom-capture-markercheck`, from
2026-08-19T14:54 local. Started with `restart-headroom-capture.sh`, which
restarts the **running** binary and changes only `HEADROOM_CAPTURE_DIR`, so the
build is the same one windowgap was captured on and the marker levers are the
only thing under test. Disarm by restarting without it.

Note windowgap is itself a clean single-build corpus: all 1,159 turns fall
between 20:20 and 01:51, after the 2026-08-18T20:20 binary was installed. That
is the binary still running, so markercheck is a straight replication of
windowgap's build on fresh traffic — it answers "is the 2pp real or is it that
corpus", not "does it survive the four commits". Nothing has been installed.

Inter-turn gaps have a median of 9 seconds. Only 1.0% of blindguard gaps and
1.5% of windowgap gaps exceed the 5-minute TTL; two of 8,681 exceed an hour.
Any lever aimed at idle-gap cache expiry has almost nothing to catch here.

## Proved this session

**The `--exclude-tools` list was costing 5pp for nothing.** The previous
revision of this document called that cost legitimate. It was not. The
exclusion exists so the model never acts on a *summary* of a file it is about
to edit, which holds for the live-zone compressors — they rewrite a result and
keep no original. Offload keeps the original: the block becomes a digest and a
preview and the bytes come back through `headroom_retrieve`.

Checked before touching it: 10,415 digest-to-content round trips against the
client's own re-sent bytes, all byte-identical, none missing, no dangling
digests across 691 bodies, `Read` results among them.

| arm | blindguard API | blindguard sub | windowgap API |
| --- | --- | --- | --- |
| `offload-gated-2000` | −6.6% | −10.1% | −29.4% |
| same, `--exclude-tools` lifted | **−11.4%** | **−13.9%** | **−32.7%** |

The `blindguard sub` column is re-priced under the fitted weights; it read
−1.5% and −13.3% under `read=0.0`. Note what moved: the gain from lifting the
exclusions is now 3.8pp, where the old weights made it look like 11.8pp. The
lever still works and still ships, but it is worth about a third of what the
earlier number claimed.

Lifting only the `--exclude-tools` half scored **byte-identical** to lifting
both lists, on all four runs. So `is_verbatim_excluded` costs nothing and
stays — those results break when their bytes change at any distance. Shipped in
`4f223054`.

Consequence worth knowing: `stale_margin` lost its old job. Its only remaining
reader is the near-tail band, `distance < margin + window`, so it and
`stale_window` now simply add.

**Claude Code's message breakpoint is sometimes short of the tail.** It spends
three of Anthropic's four markers — two on `system`, exactly one on the
messages — and that one is already on the final content block on 97% of
requests. Moving it forward on the rest is worth −0.9% API and −0.9%
subscription on blindguard against the live-proxy arm (−3.5% subscription before
the weights were fixed), and exactly zero on windowgap where the client had
already placed it at the tail on 384 of 389 requests. No-op when the marker is
right, so traffic that does not need it pays nothing. Shipped in `e796ee3c`
behind `--cache-tail-breakpoint`, default on.

**Roughly one buffered `headroom_retrieve` in five never reached the model.**
Three holes, all quiet, all fixed in `95df41d0`:

- a turn mixing the retrieval with a real client tool call could not run a
  continuation, so the old code dropped the retrieval and handed the client a
  `tool_use` for a tool it never declared. Now the content is spliced in as
  text and the real tool call goes back untouched.
- a refused continuation was a single attempt. Now two retries with backoff on
  transport errors, 5xx and 429; 4xx is left alone.
- a hash the model mistyped fell out of `parse_tool_call` as "not a CCR call".
  Now probed raw, recognised, and answered with an error.

The tracker cap went 100 → 512. Over 870 requests in 22 sessions the digests
referenced inside one 300s age window peaked at 91 against a cap of 100 — it
evicted 3,953 times in a day, 460 hashes more than once. Lifting the tool
exclusions makes roughly 1.8x as many blocks eligible.

**Overload outages run far longer than the retry budget.** Anthropic reports
overload inside a 200 body when the client asked for a stream. Over five days
of logs the loop gave up on 77 turns; clustered, the outages ran 27 to 245
seconds across 15 bursts, worst case 20 lost turns over four minutes. Against
a 3-attempt budget — about three seconds of waiting.

| attempts | waiting | turns cleared |
| --- | --- | --- |
| 3 (before) | ~3s | 16 / 77 (21%) |
| 5 | ~15s | 30 / 77 (39%) |
| **6 (now)** | **~31s** | **53 / 77 (69%)** |
| 7 | ~61s | 57 / 77 (74%) |
| 9 | ~121s | 68 / 77 (88%) |

Shipped in `8dc23eb9` as `--retry-overload-max-attempts`, default 6, separate
from `--retry-max-attempts` so nothing else waits longer. The overload branch
can afford it because the error is the *first* SSE event: nothing has been
forwarded, so a re-send cannot duplicate output.

**Transport drops mid-stream now retry.** Of 111 streams that ended without
`message_stop` across the live log and its four rotations, 32 died to a
transport drop — `error decoding response body`. The turn had barely started.

Checked before building anything: of the 18 drops that correlate to a
`stream_incomplete` event in the surviving rotations, **all 18 had a content
block open**, with 1 to 20 output tokens already parsed — median 3. So the
client had already seen `message_start`, `content_block_start` and at least one
delta every single time. A blind re-send would have spliced two different
generations together, and any design that tests "has a delta gone out yet"
would have declined to retry all 18.

So the fix holds the opening bytes back instead. While the held buffer is under
`--retry-stream-hold-bytes` (default 2048) the response is uncommitted, and a
drop there discards the buffer and issues a fresh request; the client sees one
clean stream. Once the buffer flushes the response is committed and a later
drop propagates exactly as before. This extends the safety condition at
`proxy.rs:4124` rather than working around it — the condition was always
"nothing has been forwarded", and the hold is what keeps that true for longer.

The wrapper sits below both the CCR rewriter and the telemetry tee, so a
discarded attempt is invisible to billing. `sse/stream_retry.rs`, wired at
`proxy.rs:4400`. Note the drop arrives as reqwest's `Decode`, not `Body`, so
the shared `is_retryable_transport_error` does not match it — that miss is why
`is_retryable_drop` exists.

The cost is time to first paint: 2 KiB of every stream now arrives in one burst
rather than token by token. 2 KiB covers the preamble plus roughly a dozen
deltas, which is past every drop observed. Tests in
`tests/integration_stream_drop_retry.rs` pin the retry, the committed
pass-through, the short-body flush, and the disabled case.

## Disproved this session

**The fourth breakpoint is worthless.** Claude Code leaves one of Anthropic's
four markers unused and it is tempting to spend it. Swept at seven fractions
from 2% to 50% back through history, on both corpora, under both weightings:
every arm came out **byte-identical to the untouched proxy**.

The mechanism: every turn writes a cache entry at its own tail, so a
conversation already carries a ladder of readable prefixes from its past turns.
An extra marker lands on a rung that exists. It would pay only when history
below the tail is edited, which is rare enough here not to register.

Spending it on a 1h anchor deep in history — the one thing the 5m ladder cannot
survive — was also measured. 0.2pp at best, inside the noise between fractions,
because idle gaps past 5 minutes are 1.0–1.5% of turns.

**The third tail breakpoint does not survive contact with the budget.**
`tail-breakpoints-3` scored −16.9% subscription on windowgap against the live
proxy's −15.1% and looked like the best untried idea in the file. It asks for
**five** markers on 1,133 of 1,159 requests: the client already spends all four,
two on `system` and two on the message tail. The simulator was quietly dropping
the earliest to fit, so its score was never "three tail markers" — it was "one
system marker and three tail", a different request that nobody had named.

Named and measured as `rebalance-1sys-3tail`, which asks for exactly four, with
`rebalance-1sys-2tail` as the control that pays the system marker without
spending it:

| arm | windowgap sub | blindguard sub |
| --- | --- | --- |
| live proxy (2 sys, 2 tail) | −15.1% | −1.3% |
| `rebalance-1sys-2tail` | −14.7% | −1.2% |
| `rebalance-1sys-3tail` | **−16.7%** | **−1.3%** |

The split is clean on windowgap: giving up a system marker costs 0.4pp, the
third tail marker buys 2.0pp, net 1.6pp. On blindguard the whole thing is a
wash — 9,022 tokens the wrong way, which is nothing on 216M. A lever worth
1.6pp on one corpus and zero on the other is not a ship, and the corpus where
it pays is the smaller and newer of the two.

`cachesim.py` now counts requests that go over `MAX_BREAKPOINTS` and prints
`OVER BUDGET` against the arm. The comment there had claimed for a while that
over-budget requests were "flagged rather than fixed"; they were not flagged at
all, which is how a five-marker arm came to look like the best idea in the file.

**`pair-back-05` was measuring something else.** The arm that suggested the
fourth breakpoint paid was clearing every message marker and re-placing two
with `ttl: "1h"`. Three levers in one arm. Separating them, the tail move
carried all of it: `shipped-tail` alone scored −3.5% subscription on blindguard
against `pair-back-05`'s −3.3%.

The premise underneath it was wrong too. An earlier note in this work claimed
Claude Code places two message breakpoints one message apart at 99.4% and 100%
of history. Counting the captures directly: **one** message breakpoint, 7,699
of 7,839 on blindguard and 997 of 1,009 on windowgap, and it is at the tail.
Count the markers before building on a claim about where they are.

## Still open

**The two markers we control go out one block apart, and nobody had tested
moving them.** The disproof above is about the *client*: it sends one message
marker. What goes upstream carries two, because the proxy adds its own tail
one — 599 of 600 sampled blindguard turns run `2sys+1msg -> 2sys+2msg`. So the
wire spends all four markers, and the two we own sit **one block apart** on 1,110
of 1,142 windowgap turns and 1,342 of 1,477 blindguard turns. That is 0.35% and
0.67% of history: the case `_tail_breakpoints`' own comment calls worthless,
because adjacent markers cache nearly the same prefix.

Corpus-wide counts confirm the doubling: production writes 15,446 message
breakpoints against the client's 7,699, exactly the extra one per turn.

But it appears to cost almost nothing. The `1h`→`5m` rewrite above kept the
extra breakpoint and still landed on -4.8%, the same number as a replay that
never adds one. So the whole API-side loss is the TTL, and the redundant
adjacent marker is waste that does not show up in dollars — worth removing for
clarity, not for savings, and worth measuring on its own before anyone spends
a session on it.

`shipped-tail-back-05` looks like it tested this and did not. `_spread_shipped`
skips any request whose message markers are not exactly one, which is true of
the client and false of `--base forwarded`. It skipped every request and scored
byte-identical to the live proxy. That read as "no effect"; it was "never ran".
Anything else guarded on the client's marker count has the same hole.

`spread-wire-*` moves the earlier of the two, carrying its TTL rather than
rewriting it, so it is one lever and not three:

| back | windowgap sub | windowgap API | blindguard sub | blindguard API |
| --- | --- | --- | --- | --- |
| live proxy | −15.1% | −15.9% | −1.3% | +1.8% |
| 2% | **−17.2%** | **−18.5%** | −1.2% | +2.0% |
| 5% | −17.1% | −18.4% | −0.9% | +2.3% |
| 10% | −16.6% | −17.7% | −0.8% | +2.4% |
| 25% | −14.8% | −15.4% | −0.8% | +2.4% |

A clean peak at 2-5% on windowgap, worth about 2.1pp subscription and 2.6pp
API, and both weightings agree on the shape. Push the marker further than 10%
and it turns into a loss. On blindguard every distance is worse than doing
nothing, monotonically worse the further back it goes.

**Two levers in a row now split the same way** — this and
`rebalance-1sys-3tail` both pay about 2pp on windowgap and lose on blindguard.
That is worth more attention than either lever. Blindguard is the superseded
build and windowgap is the current one, so "fails on blindguard" may mean
"fails on behaviour the proxy no longer has" rather than "does not work". The
way to settle it is a third capture on the current build, not more arms. Until
then neither should ship.


**The 6.6pp gap between model and production, on the old corpus.** Like for
like — same floor, same policy — the modelled policy was worth −4.8% and the
proxy running it delivered +1.8%. The lead was that the near-tail window looked
inert in production, at about 1 window offload per 11 turns.

**That lead was a measurement error, and both halves of it are now dead.**
`src/bin/offload_replay.rs` replays a corpus through the real
`offload_anthropic_request`, with the real gate and the real drift detector,
and reports the counters the proxy only ever logs. Run it as:

```
cargo run --release -p headroom-proxy --bin offload_replay -- <capture_dir>
```

First: the window is not inert. Aggregated over the whole log span
(2026-08-17T10:03 to 08-19T09:55, 8,287 turns carrying a qualifying block),
production fires 1,742 window offloads — **1 per 4.8 turns**. The replay agrees
independently: 1 per 4.7 on blindguard, 1 per 5.4 on windowgap. The old "1 per
11" divided a window-offload count from one span by a turn count from a wider
one.

Second: lifting the exclusions does not drain the deferral backlog. Replaying
blindguard on `4f223054` against its parent, everything else held:

| counter | before | after |
| --- | --- | --- |
| `blocks_offloaded` | 72,349 | 73,181 |
| `blocks_deferred` | 18,453 | 18,441 |
| `window_offloads` | 1,681 | 1,673 |
| `tokens_saved` | 95,335,968 | 97,392,897 |

So the commit is worth about +1.1% blocks and **+2.2% tokens saved** — real, and
in the same direction as the bench arms, but it is not the step change the
earlier note predicted. Deferrals do not move. Blocks do not start converting
at distance 0-1; they convert at much the rate they always did.

Caveat on the absolute numbers: the replay sees a rebuild boundary on 3.1% of
turns (245 of 7,839) where the production snapshot reported 0.16%. That gap is
unexplained. It does not affect the before/after comparison, which holds the
detector fixed across both runs, but do not read the absolute deferral counts as
production truth until it is chased down.

**Closed: it was `--force-1h-cache-ttl` (2026-08-21).** Not a defect, and not
in the offload gate at all — the measured price of a setting the model never
simulated.

`offload_replay --out DIR` now dumps the pre-gate body as `req-*.json` and the
post-gate body as `out/<request_id>.json`, so `cachesim.py compare` prices both
arms with one function. The real gate replayed over blindguard bills **-4.8%**,
matching the model exactly. The replay runs `offload_anthropic_request` and
nothing else, so the whole 6.6pp lived in the stages it skips.

Confirmed directly by rewriting production's own forwarded bodies from `1h` to
`5m` and changing nothing else: **+1.8% becomes -4.8%**, the replay's number.
Marker counts show why — production writes 15,446 message breakpoints against
the replay's 7,699, and 15,400 of 15,400 system breakpoints at 1h where the
client mix has 3,670 at 5m. `--force-1h-cache-ttl true` is set at
`~/.headroom-flags.sh:232`; the code default is `false` (`config.rs:2425`).

The setting is a straight trade between the two pricing axes:

| | API | subscription |
| --- | --- | --- |
| production, 1h | +1.8% | -1.3% |
| same bodies, 5m | -4.8% | +2.7% |

1h wins by 4.0pp on subscription and loses by 6.6pp on API, which is what
`cache_ttl.rs:20-30` says it should do: writes count at raw token count for
rate limits with no TTL distinction, while a 1h write costs 2x base input
against 1.25x for 5m. Keep it while paying by subscription; turn it off on
API. Note the swing is not just the multiplier — at 5m the entries expire
sooner and creates rise 30.8%, and 5m still wins on dollars even paying for
those extra writes.

Two things this leaves alone. The 3.1%-vs-0.16% rebuild-boundary discrepancy
below is still unexplained, and the replay reproduced 245 boundaries of 7,839
(3.1%) again. And `cachesim`'s `offload-gated-*` strategies still model the
gate alone, so any future arm compared against production carries the same
blind spot — dump and compare rather than trusting the strategy number.

**Pricing is ruled out as a source (2026-08-21).** `pricing.rs` had Opus 5 at
the retired Opus 4.1 rates — $15/$1.50 per MTok against the real $5/$0.50, a
3x overstatement, corrected in `85c32900`. That inflated the savings ledger
(today's booked $9.02 is really $3.12) but it cannot touch this gap. Both
sides of the comparison are weighted token counts, not dollars: cachesim's
weights are relative to fresh input = 1.0, and `bench/cachesim.py` contains no
dollar arithmetic at all — the only `usd` under `bench/` is `ab_replay.py:123`
reading captured `costUSD`, a different tool. So the 6.6pp is a decision
divergence, not a pricing-formula difference, and the harness below only has
to explain where the decisions differ.

**The five `workspace`-partition memories are placed (2026-08-19).** The partition
is now empty. Beacon, AWS and CLAUDE.md went to `default`; Cadence went to
`default::cadence-0000000000000000`; flaky_tests was deleted as stale — its
whole payload was "fixed as of 2026-03-24, PROJ-519".

`default` is the right home for cross-repo reference because
`router::shared_partition` strips the `::project` suffix, so a record stored
under plain `default` is visible from every project partition (`ctx_backend.rs`
line 140). Beacon is queried from team-analytics, team-analytics-insights and
reports-automation, so no single repo partition would have served it.

The store is `~/.claude-work/context-mode/memory/memories.db`, not
`~/.headroom/memories`, which is empty. `user_id` is carried twice, in the
column and inside the record JSON — update both or reads go inconsistent.

## Ruled out earlier, still ruled out

**The PR-J4 boundary gate withholds nothing.** With the boundary requirement
and without it, −6.6% either way, 0.01% apart.

**Stripping old thinking blocks.** Scores −3.2% and is unshippable.
`restore_client_reasoning_blocks` in `proxy.rs` compares outbound signed
`thinking` and `redacted_thinking` blocks against the client's and restores the
whole message array on any mismatch — a deletion is a mismatch, so the arm
would be reverted every turn. The guard exists because Anthropic rejects the
turn outright. Do not fight it.

**Byte-based prompt composition.** Images bill by dimensions, not base64 length
— 9.3–11.1 bytes per token against 3.1 for text — so the 2 MB image bodies are
the cheap ones. Price tokens before claiming what dominates a prompt.

## Reproducing

```
cd bench
python3 cachesim.py experiment ~/headroom-capture-blindguard \
    --weights subscription --base forwarded \
    --strategy offload-gated-2000 --strategy offload-gated-2000-no-tool-list
python3 cachesim.py damage ~/headroom-capture-blindguard \
    --base forwarded --strategy offload-gated-2000 --top 3
python3 fit_weights.py fit --window 5h
cargo run --release -p headroom-proxy --bin offload_replay -- \
    ~/headroom-capture-blindguard
```

`fit_weights.py fit` defaults to the 7d window, which cannot resolve the read
coefficient. Pass `--window 5h` for the fit the weights come from.

`--base forwarded` stacks each arm on what the proxy already did, so the number
is incremental over the live build. `damage` takes one `--strategy` per run and
diffs against what the client sent; read it on anything that scores well,
because `experiment` prices cache structure only and deleting the conversation
scores beautifully there.

Arms added this session, all in `strategies.py`:
`offload-gated-2000-no-tool-list` (the shipped exclusion change),
`shipped-tail` and `shipped-tail-back-05` (the shipped breakpoint move),
`shipped-back-*` and `anchor-1h-*` (the disproved fourth breakpoint),
`spread-wire-*` (the two wire markers moved apart), `rebalance-1sys-3tail` and
`rebalance-1sys-2tail` (a system marker traded for a third tail one).

Two harness notes.

The gated arms are session-aware. They carry a monotonic per-session set across
turns, so `strategies.reset()` runs between arms; a stateful strategy takes
`(body, turn)` and `apply` passes the turn when the signature asks for it.

`_is_rebuild_boundary` does **not** infer boundaries from the body. Inferring
them — any change at a position both turns share — called 98.5% of turns
boundaries, because Claude Code rewrites its own reminders on nearly every
turn. The live counter says 0.16%.

## Traps that cost hours

**Do not run a whole-corpus bench mode against blindguard without a cap.** It
used to load the corpus, its forwarded twin, and one strategy's copy at once —
7.8 GB on disk, far more parsed — and it took the whole machine down, not just
the process. `compare` and `experiment` now stream the corpus a turn at a time
and peak at about 60 MB, so blindguard runs in one pass in ~2.5 minutes. The
other modes (`score`, `defects`, `damage`, `validate`) still materialise
everything and are still unsafe there. Cap anything you are unsure of:

```
(ulimit -v 8000000; python3 cachesim.py ...)
```

The streaming rewrite is byte-identical on windowgap against the old code, for
`compare` under both weightings and for `experiment`. Two things it cannot do:
the cache scope is model plus credential and deliberately spans sessions, so
the corpus cannot be chunked by session; and strategy state is module-global,
so arms run one after another rather than in lockstep down one pass.

### Older traps


**Count markers, do not assume them.** See `pair-back-05` above.

**`json.dumps` escapes non-ASCII by default**, inflating byte counts 7–12%.
Use `ensure_ascii=False` and `.encode()`. With that fixed the capture's
forwarded bytes matched the proxy's own figure exactly, 103,013,808.

**Strip `cache_control` before diffing bodies for prefix stability.** Markers
move to the new tail each turn and register as content divergence. Leaving them
in reported p50 100% invalidation; stripping them inverted the result to 0.098%
for the proxy against 0.526% for the client.

**SQLite returns BLOB.** Comparing a Python `bytes` repr against text reported
0.41% fidelity. Decoding first gave 100.00% across 10,415 round trips.

**`nohup ... &` in a background shell returns immediately** and reports a
completion that has not happened. Use an `until ! pgrep ...` loop.

## Uncommitted

New this session: `crates/headroom-proxy/src/sse/stream_retry.rs`,
`crates/headroom-proxy/tests/integration_stream_drop_retry.rs`,
`crates/headroom-proxy/src/bin/offload_replay.rs`, plus `config.rs` for
`--retry-stream-hold-bytes` and the `proxy.rs` wiring.

`bench/cachesim.py`, `bench/fit_weights.py`, `bench/strategies.py`, the four
files under `crates/headroom-proxy/src/memory/`,
`crates/headroom-core/src/persistent_metrics.rs`,
`crates/headroom-core/src/savings_tracker.rs`,
`crates/headroom-proxy/src/proxy.rs`,
`crates/headroom-proxy/tests/memory_real_corpus.rs`,
`docs/subscription-optimization.md`. Outside the repo: `~/.headroom-flags.sh`
and `~/restart-headroom.sh`.

The `memory/*`, `persistent_metrics.rs` and `savings_tracker.rs` changes
started out as older work left alone, and the four commits were staged hunk by
hunk to keep them out. Three of those files have since been touched
deliberately and carry work of their own:

- `savings_tracker.rs` — the `history_rendered` mirror and the in-place trim,
  above.
- `memory/handler.rs` — eight instrumented early returns in
  `search_and_format_context`, each naming why it declined
  (`inject_context_off`, `mode_is_tool`, `no_backend`, `no_user_query`,
  `empty_query`, `search_failed`, `no_results`, `all_below_min_similarity`),
  plus the `tail_anthropic_reaches_a_short_conversation` regression test.
- `proxy.rs` — the `frozen` fix at the memory injection site. It was passing
  the length of the *system* array where the callee wanted a count of
  messages, so two system blocks made it skip `messages[0..2]`.

Two tests fail and are not ours: `passthrough_preserves_cache_control_markers`
and `passthrough_recorded_fixture_byte_equal_sha256`, both in
`integration_compression`, bisected to `6a43a6b5` and clean at `579047f3`.
Confirmed pre-existing again this session by stashing every change from it and
getting the identical two. Everything else passes: `headroom-proxy --lib`
1,598, `headroom-core --lib` 2,031, `ctx_cache_stability` 10,
`integration_inband_sse_retry` 5.

## The pre-4f223054 replay arm

`/tmp/hr-before` is gone. To rebuild the "before" side of the offload A/B,
check out `e796ee3c` into a worktree, copy `offload_replay.rs` over, and put
this field back in the config literal — `4f223054` deleted it, and without it
the old build's gate never fires:

```rust
// Restored for the pre-4f223054 build: this is the field that commit
// deleted, carrying the same default list the proxy shipped.
exclude_tools: headroom_core::tool_exclusion::DEFAULT_EXCLUDE_TOOLS_CSV
    .split(',')
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .collect(),
```

## The marker levers do not replicate (2026-08-19)

Backtested `spread-wire-02`, `spread-wire-05` and `rebalance-1sys-3tail` on the
three older corpora that carry two message markers, `--weights subscription
--base forwarded`. Live proxy vs the arm, as `vs claude code`:

| corpus | turns | live | spread-02 | spread-05 | rebalance-1sys-3tail |
|---|---|---|---|---|---|
| capture-beta  | 1869 | +69.4% | +69.5% | +69.5% | +143.4% |
| toolblocks | 374 | +32.2% | +32.3% | +32.4% | +49.4% |
| msg0      | 231 | +67.2% | +67.1% | +62.1% | +106.3% |

`spread-wire` is flat to a shade worse on the two larger corpora. Only msg0, the
smallest, shows the 5pp gain, and one corpus out of four is what noise looks
like. The ~2pp windowgap win does not survive contact with other traffic.

`rebalance-1sys-3tail` is worse than flat — it is bad everywhere, by 17 to 74
points. Its uncached share goes to 0.0%, which is the tell: it rewrites the
cache every turn, and writes cost more than reads. Drop it.

Neither should ship. markercheck can still confirm on the current build, but the
prior has moved: expect no gain.

Corpora with one message marker — drift, replay-on, replay-off — cannot test
this family at all. `_marked_positions` needs exactly two and skips the request
otherwise, so every turn would be silently skipped and the arm would score
identical to live.

### markercheck settles it (2026-08-19, 446 turns)

Same build as windowgap, fresh traffic, `--weights subscription --base forwarded`:

```
claude code                 8,529,901    +0.0%   uncached 0.5%
live proxy                  7,410,026   -13.1%   uncached 0.4%
spread-wire-02              7,426,398   -12.9%
spread-wire-05              7,429,335   -12.9%
spread-wire-10              7,433,292   -12.9%
rebalance-1sys-3tail        7,422,759   -13.0%   uncached 0.0%
```

Every arm is worse than the live proxy. Not flat — worse, on the one corpus
captured specifically to test them, on the build windowgap ran on.

That is four corpora out of five saying no gain (capture-beta, toolblocks,
markercheck flat-to-worse; msg0 the lone gain at 231 turns). windowgap is the
outlier, not the signal. **Both levers are closed.** Do not re-open without a
reason that explains why windowgap differed.

Worth noting how well-cached markercheck is: 0.4% uncached against capture-beta's
49.2%. On traffic the proxy already handles well there is no marker slack left
to win, which is the most likely reason every arm costs a little.

**The baseline is the old placement.** The `live proxy` row is the binary
running since 2026-08-18 20:20, which predates `e796ee3c` (message breakpoint
moved to the last content block). The arms lost to the placement that shipped
before that commit. Nothing here reopens — the margin was 0.1-0.2 pp and four
corpora agree — but rebaseline before re-running these arms against a build
that carries e796ee3c. On markercheck-like traffic that rebaseline should come
out flat: e796ee3c is a no-op when the marker already sits at the tail, and at
0.4% uncached it nearly always does.

## Memory layer: initialised, retrieving nothing (2026-08-19)

**Personal and work sessions share one store.**
`~/.claude-personal/context-mode` is a symlink to `~/.claude-work/context-mode`,
and the proxy runs with `--ctx-store-dir ~/.claude-personal/context-mode`. One
physical store, both paths. `~/.headroom/memories` is a decoy — that is
`default_native_memory_dir()`, only used when `use_native_tool` is on, and it is
empty.

**Nothing is being injected.** Across 1,929 captured forwarded bodies
(markercheck 769 + windowgap 1,160) there are zero `## Relevant Memories`
blocks. Cross-checked by grepping the same bodies for all 434 memory UUIDs: the
ids that do appear appear in the client body the same number of times, so they
are conversation text, not injection. The outbound capture at `proxy.rs:3914`
runs after the inject site at `proxy.rs:3191`, so an injection would show.

Recall injection works over the same plumbing — 795 `<session_recall>` blocks in
the same corpus — so the budget and the rewrite path are sound.

Ruled out, each checked directly:

- injection budget — recall blocks peak at 4,060 bytes against 8,192, and
  `injection_budget_overrun` never fires
- ~~`min_similarity` — score is `|rank|/(1+|rank|)`; real BM25 ranks in this
  index are -3.6 to -9.7, giving 0.78-0.91 against a 0.3 floor~~ **This was the
  cause. See the resolution below — the check queried the FTS table directly
  and never saw the number the search path actually produces.**
- index completeness — all 434 records are indexed, 3,422 chunks
- FTS5 syntax — a raw sentence does throw `syntax error near ","`, but
  `store.rs:630` sanitizes before matching
- validity — all 434 have `valid_from` in the past and `valid_until` null
- config — `inject_context` and `memory_mode` default on and to `auto_tail`;
  `HEADROOM_MEMORY_INJECT_TOOLS=0` only disables tool injection
- enclosing gates — the chain to `proxy.rs:3191` is `should_intercept` -> parse
  -> `Ok` arm, nothing else
- the rewrite — `changed = true` is set and the body is re-serialized

So `search_and_format_context` returns `None` on every turn and the reason is
not visible from outside.

**Instrumentation added (2026-08-19).** Every early return in
`search_and_format_context` now emits `event = "memory_inject_skipped"` at info
with a `reason`: `inject_context_off`, `mode_is_tool`, `no_backend`,
`no_user_query`, `empty_query`, `search_failed`, `no_results`,
`all_below_min_similarity`. The last two carry `user_id`, and
`all_below_min_similarity` also carries `found` and `min_similarity`, which
separates "the partition had nothing" from "the partition had hits and the
floor ate them". After the next restart:

```
grep -o '"reason":"[a-z_]*"' ~/headroom-proxy.log | sort | uniq -c
```

one turn names the branch.

**Separate bug, fixed.** `proxy.rs:3245` computed `frozen` as the length of the
**system** array and passed it to `append_to_latest_user_tail` as
`frozen_message_count`, which indexes into **messages**. Two system blocks
skipped `messages[0..2]`, so a conversation one or two messages long had no
eligible tail and got no memory — silently, since the callee just returns 0
bytes. Now passes 0, which is the honest value: the real frozen boundary comes
from the prefix-replay tracker, and that does not run until
`apply_prefix_replay` several hundred lines later. The guard is inert either
way, because the callee walks backwards for the last user message and the turn
being sent is by definition not in the cached prefix. Pinned by
`tail_anthropic_reaches_a_short_conversation`.

This is a real bug but not the one that matters — it only covers the opening
turns. It does not explain 1,929 turns of nothing.

### Resolved: the score could never reach the floor (2026-08-21)

It was `min_similarity` after all, and the check above is why it took two days
to see. That check ran BM25 against the FTS table directly and got ranks of
-3.6 to -9.7. The search path does not return those. `CtxStore::search` fuses
two ranked lists and overwrites `SearchHit::rank` with the **negated RRF
score** (`store.rs`, `rrf_search`), and with `RRF_K = 60` the best hit
obtainable is `2/(RRF_K + 1)` = 0.033. Through `|rank|/(1+|rank|)` that capped
the output at **0.032** against a floor of **0.3**.

So no result could ever pass, for any query, at any threshold setting above
0.032. Confirmed in the live log: 7,265 consecutive `all_below_min_similarity`
events, each reporting ten results found, and not one success.

Fixed in `3cee05d1` by scaling the fused score by `RRF_K + 1` before the
squash: a single-list leader now scores 0.5, a both-list leader 0.67, and
roughly the first fifty fused positions clear 0.3. Tests pin the property that
broke — the best hit RRF can produce must be retrievable.

The trap worth remembering: `SearchHit::rank` is named for BM25 and carries a
fused score. Anything reading it has to know which scale it is on, and testing
the index directly will not tell you.

Two things this does not cover. The live proxy had no `HEADROOM_MEMORY_*` in
its environment, so it ran `auto_tail` rather than the configured `tool` mode —
`claude-launcher` sources the flags file only in the branch that starts the
proxy, so a run reusing a live proxy exports nothing. And the backend's own
tests never caught the scoring bug because they assert on presence and
ordering, never on absolute score against the threshold.

## Allocator swap: measured, rejected (2026-08-19)

Do not try this again without reading this section.

A synthetic benchmark said glibc was the problem. Parsing a real 908 KB
captured body on 20 threads, parse-and-drop, three runs each:

| allocator | elapsed | RSS retained after all values dropped |
|---|---|---|
| glibc | 0.90s | +230 MB |
| jemalloc | 0.57s | +65 MB |
| mimalloc | 0.52s | +228 MB |

jemalloc looked like a 3.5x memory win that was also faster. It was wired in
behind `cfg(not(target_env = "msvc"))` — CI builds `x86_64-pc-windows-msvc`,
which jemalloc does not support — and it built and passed all 1,598 tests.

Then it was measured on the real proxy: 60 captured bodies replayed through a
dummy upstream, with ctx-offload, ctx-inject, memory, compression and
prefix-replay all on, stores isolated to `/tmp`, RSS sampled every 10s for a
minute after the load stopped.

| build | start | peak | settle, 10s to 60s |
|---|---|---|---|
| glibc | 32 MB | 290 MB | 290 290 290 290 290 290 |
| jemalloc, default | 32 MB | 325 MB | 325 325 325 325 325 325 |
| jemalloc, `background_thread:true,dirty_decay_ms:2000` | 34 MB | 312 MB | 312 312 312 312 312 312 |

jemalloc was **worse**, by 22-35 MB, and no configuration recovered it. The
change was reverted.

The flat settle curve is the real finding. Nothing decays because nothing is
waiting to be freed — the proxy's RSS is **live data**, held on purpose by the
replay store, the offload store, the CCR tracker and the semantic cache. The
synthetic benchmark measured pure allocate-and-free churn, which the proxy does
not actually do at that scale; it measured a problem the proxy does not have.

So there is no free memory win here. The levers that would work all trade cache
coverage for bytes — the replay store alone is a 1,000-session LRU
(`prefix_replay.rs:86`) sized by count, not by bytes — and that changes
behaviour, which was explicitly out of scope.

Lesson worth keeping: a microbenchmark that models the wrong allocation
lifetime will confidently recommend the wrong fix. Measure the real process.

## Where the proxy's own latency goes (2026-08-19)

Earlier in this file I wrote that speed was not worth chasing, on the grounds
that a request takes ~2,000 ms and the redundant JSON work is ~5 ms. That was
measured against **total** latency, which is upstream inference. It was the
wrong denominator.

Measured properly — captured bodies replayed through the proxy against an
instant local upstream, so the number is the proxy's own cost:

| body | proxy overhead |
|---|---|
| 198 KB | 156 ms |
| 445 KB | 156 ms |
| 674 KB | 171 ms |
| 858 KB | 239 ms |

Fits `~61 ms fixed + 0.155 ms/KB`. **89% of it is proxy CPU**, not waiting
(184 ms CPU against 207 ms wall over 20 requests). On the median 410 KB body
that is roughly 125 ms per request, about 6% of a 2 s turn.

Ruled out by measurement, so nobody re-checks: **tokenization** (0.9 ms for
710 KB — Claude models resolve to the estimator, not BPE), **fsync** (2.3 ms on
this filesystem), and **quadratic scaling in message count** (cost per message
*falls* from 4,168 us at 20 messages to 361 us at 586).

Phase timers on a 743 KB body, since no profiler is available here (`perf` and
`gdb` are both absent):

```
ident            1.8 ms    session identity + drift key
cache            1.0 ms    semantic cache check
ctx              0.0 ms    (offload/inject/memory off in this run)
decision         2.5 ms    compression decision
compress        39.8 ms
outcome-context  7.5 ms    the re-parse whose comment calls it "cheap"
replay           2.0 ms
tool stages     54.3 ms    <- mislabelled, see the correction below
                -------
total          108.6 ms    to the send point
```

### Correction: it was the savings tracker, not the tool stages

That 54.3 ms row is a window, and the window was drawn too wide. It spans
`record_request_footprint` (`proxy.rs:2108`), which runs at the end of it.
Split the window and the four tool stages account for **3.9 ms** between them;
the footprint call is **49.2 ms** of the 53.2 ms.

`record_request_footprint` calls `record_proxy_overhead` and `record_tools`,
and each one calls `SavingsTracker::save`. `save` rebuilt every `history`
entry into a `Value` before serialising. On a 5,000-entry history that is
1.14 MB of the 1.4 MB payload, rebuilt from scratch, several times per
request — a typical request reaches four or five recorders.

Measured in release against the real savings file:

| | before | after |
|---|---|---|
| `record_proxy_overhead` | 12.40 ms | 1.16 ms |
| `record_tools` | 11.96 ms | 1.33 ms |
| `record_request` | 16.13 ms | 2.22 ms |

History is the whole of it: empty the `history` array and the same pair of
calls costs 0.62 ms instead of 24.36 ms.

Fixed in `savings_tracker.rs`. `State` carries `history_rendered`, the history
already rendered, kept in step by `push_history` and `trim_history`. `save`
serialises from a borrowed struct rather than assembling a `Value`, so the
history is neither rebuilt nor cloned. `trim_history` retains and drains in
place; it used to clone every surviving entry into a fresh vector on every
push. The file it writes is unchanged — same key order, identical history, and
the only keys that move are the counters the recorder touches.

### What is left of the parse-once idea

`maybe_compact_tool_schemas` (`proxy.rs:1781`) does parse the whole body, edit
the `tools` array and re-serialise it, and `maybe_stabilize_tool_order` and
the tail-breakpoint stage do the same. That is still redundant. It is worth
**3.9 ms**, not 54, so price it accordingly before touching the plumbing.

The second fix once listed here — memoise tool-schema compaction on a hash of
the `tools` array — **already exists**: `cache_key`/`cache_get`/`cache_put` in
`tool_schema_compaction.rs:367`. The 0.6 ms that remains is the cache *hit*
path, canonicalising the tools array to build the key and cloning the cached
value out of the mutex. Nothing is recomputed.

The 7.5 ms outcome-context re-parse at `proxy.rs:3510` can reuse an existing
parse. Its comment — "Re-parses `buffered` for model/num_messages (cheap,
happens once)" — is wrong twice: it is 7.5 ms, and it is the fifth parse.

Compression at 39.8 ms is the proxy earning its keep and is not in scope here.
