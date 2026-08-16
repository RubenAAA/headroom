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
