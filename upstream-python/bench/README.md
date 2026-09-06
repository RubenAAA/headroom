# bench — measuring the proxy without spending tokens

What Anthropic bills for a request is arithmetic. Given the exact bytes and
where the `cache_control` breakpoints sit, the split into cache read, cache
creation and fresh input is fully determined. So a corpus of captured request
bodies can be priced offline, as often as you like, for nothing.

That turns "is the proxy any good" from an experiment that costs a subscription
into one that costs a few seconds of CPU.

## The corpus is the experiment

Arm the capture in `~/restart-headroom.sh` (`HEADROOM_CAPTURE_DIR=...`) and
restart. The proxy then writes two files per turn:

- `req-<epoch>-<seq>.json` — an envelope around what **Claude Code sent us**
- `out/<request_id>.json` — what **we forwarded** to Anthropic

The pair is a controlled A/B on identical traffic, already run, sitting on disk.
Both arms are scored by the same estimator, so estimator error cancels and the
comparison is far more trustworthy than either arm's absolute number.

Costs about 1 MB per request and never rotates. Disarm when done.

## Commands

```
cachesim.py compare    <corpus>              # proxy vs plain Claude Code, totals
cachesim.py defects    <corpus> [--top N]    # which turns we lose, and to what
cachesim.py damage     <corpus> [--top N]    # what we changed about the conversation
cachesim.py experiment <corpus> [--base client|forwarded]   # price ideas that never shipped
cachesim.py validate   <corpus> --log <proxy.log>           # against real billing
watch.py [--corpus DIR] [--interval 900]     # background fault watch, one line per fault
```

`defects` and `damage` are a matched pair and neither is safe alone. `defects`
prices cache structure and would happily reward a build for deleting history;
`damage` ignores cost and reports what the conversation lost. A change is good
only if `defects` improves and `damage` does not.

## Testing an idea

Add a function to `strategies.py` and it appears in `experiment`:

```python
@strategy("my-idea")
def my_idea(body):
    """One line, printed in the report."""
    return body
```

`--base client` asks what a fresh proxy would do to raw traffic. `--base
forwarded` stacks the idea on what the proxy already does, which is the right
question for a patch. `noop` must always land exactly on the baseline — if it
does not, the harness is lying and nothing else in the report means anything.

## What it cannot tell you

It models cache structure. It does not know whether the answer was any good.
Context offload, injection and compression change meaning, and no arithmetic
here will notice a conversation that got worse. `damage` reports *what* changed,
never whether the change hurt. Judging that needs a human reading the diff.

## Trust, and how it was earned

Scored against the 2026-08-14 capture (relocation build), the simulator reports
46.2% of the bill as uncached input and 22.8M fresh tokens. The independent
live-log regression on that same build measured 46% and 22.0M. Nothing was
tuned to make those agree.

Not yet calibrated in absolute terms: token counts come from a
bytes-per-token constant (3.6). `validate` joins the corpus to real
`turn_cost_ledger` lines and reports the constant that best fits. Run it before
quoting any absolute number. Ratios between arms are robust without it.

Two modelling bugs, both of which inverted the answer, both fixed — read
`_blocks` and `CacheSim.score` before adding anything:

- Reads match the longest cached prefix at **any** segment boundary. Breakpoints
  govern writes only. Modelling reads as breakpoint-aligned collapses the hit
  rate.
- The provider tokenises the rendered prompt, not the JSON envelope. `"hi"` and
  `[{"type":"text","text":"hi"}]` are the same prefix, and Claude Code flips
  between the two shapes as its marker moves.

## Pricing

`0.1x` read, `1.25x` 5m write, `2.0x` 1h write are **API** prices and are
documented. Subscription window metering is not documented anywhere, and the
open question is whether reads count at all — on the API they do not count
toward rate limits, and reads are over 90% of Claude Code's tokens. `Weights.
documented` marks which profile is a guess; `fit_weights.py` recovers the real
one by regression against `proxy_ratelimit_unified_utilization`.

That question is now answered. `fit_weights.py fit --window 5h` over 17,152
turns in five log files — 2,783 samples across 28.2 h, 1,569 intervals carrying
turns, R² 0.328 — puts **cache read at 0.10**, with a bootstrap band of
0.04–0.25 that excludes zero, and the 1h write at 1.45 with a band of
1.00–2.00. The 5m write is pinned at 1.25 to free the other two, so it
carries the API value by assumption, not by evidence. `cachesim.py` ships these
numbers as `SUBSCRIPTION`.

The fit resolves the 5h window and nothing else. Over 7d, utilization moves in
46 intervals against 361 for 5h, and the read band widens to 0.00–0.30,
which does include zero. Any 7d claim is unfitted. `fit` defaults to 7d, so pass
`--window 5h`.

Price every arm under both profiles. They disagree and the disagreement matters.
The old `read=0.0` subscription profile overstated anything that trades writes
for reads, by roughly 3x on the exclusion arm and 4x on the tail breakpoint. It
never changed which arm won, only by how much.

## Findings so far

- Relocation cost 1.74x plain Claude Code. Removed.
- Claude Code's own breakpoint placement beats repositioning it
  (`tail-breakpoints-*` all lose by ~11%).
- The fix proposed in the relocation post-mortem — split the volatile counter
  out so the stable 12 KB can cache — is **refuted**. Splitting alone does
  nothing (+0.3%): both halves still sit past the last breakpoint and both still
  bill fresh. Adding a breakpoint between them does cut uncached from 46.2% to
  2.6%, and still costs more (+148%), because it swaps 21M fresh tokens (1.0x)
  for 21M 1h writes (2.0x) that are never read back. Relocation strips that
  block from history and re-appends it every turn, so there is nothing to read.
  A cache write pays for itself after about 1.1 reads; this one got zero.
  Deleting relocation was the only fix, and that is what shipped.

  Two implementation bugs found on the way, both worth knowing before writing a
  strategy: operating only on list-shaped content applies a change on one turn
  and not the next (Claude Code flips the shape), and marking every match in
  history blows the four-breakpoint budget, silently evicting the system
  breakpoints.

- **Whether the proxy pays for itself depends on the corpus and on the
  weights.** Measured 2026-08-19 and not re-run since.

  | corpus | weights | Claude Code | proxy | delta |
  | --- | --- | --- | --- | --- |
  | blindguard | API | 237.0M | 241.2M | +1.8% |
  | blindguard | subscription | 219.7M | 216.8M | −1.3% |
  | windowgap | API | | | −15.9% |
  | windowgap | subscription | | | −15.1% |

  On blindguard the proxy is roughly a wash and which side it lands on depends
  on what you are paying with. On windowgap it is ahead under both. The
  windowgap subscription figure read −27.7% before the weights were fixed. The
  two corpora are different builds, not different luck.

## Corpora

| corpus | turns | window | build |
| --- | --- | --- | --- |
| `~/headroom-capture-blindguard` | 7,839 | 2026-08-16T21:22Z – 08-18T01:11Z | old |
| `~/headroom-capture-windowgap` | 1,150 | 2026-08-18T16:20Z onward | 2026-08-18T20:20 binary |
| `~/headroom-capture-markercheck` | 446 scored | from 2026-08-19T14:54 local | same as windowgap |

windowgap is a clean single-build corpus: every turn in it falls after the
2026-08-18T20:20 binary was installed. markercheck was armed with
`restart-headroom-capture.sh`, which restarts the **running** binary and changes
only `HEADROOM_CAPTURE_DIR`, so the build is held fixed and only the arm under
test varies. Disarm by restarting without it.

Older corpora that carry two message markers, and so can test the marker
family: `capture-beta` (1,869 turns), `toolblocks` (374), `msg0` (231). The
`drift`, `replay-on` and `replay-off` corpora carry one message marker.
`_marked_positions` needs exactly two and skips the request otherwise, so a
marker arm run there skips every turn and scores identical to live.

Inter-turn gaps have a median of 9 seconds. Only 1.0% of blindguard gaps and
1.5% of windowgap gaps exceed the 5-minute TTL, and two of 8,681 exceed an hour.
A lever aimed at idle-gap cache expiry has almost nothing to catch here.

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

`--base forwarded` stacks each arm on what the proxy already did, so the number
is incremental over the live build. `damage` takes one `--strategy` per run and
diffs against what the client sent. Read it on anything that scores well,
because `experiment` prices cache structure only and deleting the conversation
scores beautifully there.

`offload_replay` replays a corpus through the real `offload_anthropic_request`,
with the real gate and the real drift detector, and reports counters the proxy
otherwise only logs. `--out DIR` dumps the pre-gate body as `req-*.json` and the
post-gate body as `out/<request_id>.json`, so `cachesim.py compare` prices both
arms with one function. Prefer that over the `offload-gated-*` strategies when
comparing against production: those model the gate alone and carry the same
blind spot the gate does.

Two harness notes. The gated arms are session-aware — they carry a monotonic
per-session set across turns, so `strategies.reset()` runs between arms, and a
stateful strategy takes `(body, turn)` while `apply` passes the turn when the
signature asks for it. And `_is_rebuild_boundary` does **not** infer boundaries
from the body: inferring them, by taking any change at a position both turns
share, called 98.5% of turns boundaries, because Claude Code rewrites its own
reminders on nearly every turn. The live counter says 0.16%.

## Traps that cost hours

**Cap the memory before running a whole-corpus mode against blindguard.** The
old code loaded the corpus, its forwarded twin and one strategy's copy at once —
7.8 GB on disk and far more parsed — and it took the whole machine down, not
just the process. `compare` and `experiment` now stream the corpus a turn at a
time and peak at about 60 MB, so blindguard runs in one pass in ~2.5 minutes.
`score`, `defects`, `damage` and `validate` still materialise everything and are
still unsafe there. Cap anything you are unsure of:

```
(ulimit -v 8000000; python3 cachesim.py ...)
```

The streaming rewrite is byte-identical to the old code on windowgap, for
`compare` under both weightings and for `experiment`. Two things it cannot do:
the cache scope is model plus credential and spans sessions on purpose, so the
corpus cannot be chunked by session; and strategy state is module-global, so
arms run one after another rather than in lockstep down one pass.

**Count markers, do not assume them.** An arm was built on the claim that Claude
Code places two message breakpoints, at 99.4% and 100% of history. It places
one, at the tail, on 7,699 of 7,839 blindguard turns and 997 of 1,009 windowgap
turns.

**`json.dumps` escapes non-ASCII by default**, inflating byte counts 7–12%. Use
`ensure_ascii=False` and `.encode()`. With that fixed, the capture's forwarded
bytes matched the proxy's own figure exactly, at 103,013,808.

**Strip `cache_control` before diffing bodies for prefix stability.** Markers
move to the new tail each turn and register as content divergence. Leaving them
in reported p50 100% invalidation; stripping them inverted the result, to 0.098%
for the proxy against 0.526% for the client.

**SQLite returns BLOB.** Comparing a Python `bytes` repr against text reported
0.41% fidelity. Decoding first gave 100.00% across 10,415 round trips.

**`nohup ... &` in a background shell returns immediately** and reports a
completion that has not happened. Use an `until ! pgrep ...` loop.

**`fit_weights.py` must read the log rotations.** `load_turns` once read only
`~/headroom-proxy.log`, which had rotated, so samples and turns had zero time
overlap and every predictor came back R² −inf. `log_paths()` now globs the
rotations, and `fit()` bails with both time spans printed when no interval
carries a turn.
