# Savings ideas not yet captured (2026-09-03)

Source: `/home/ruben/headroom-proxy.log` and rotations `.1`, `.3`, `.4`, covering
2026-08-31 07:27 to 2026-09-03 17:42 (first and last days partial). Prices from
`crates/headroom-core/src/pricing.rs`; `claude-fable-5-1` priced at the
`claude-` fallback (sonnet-4 rates). Every turn logs `non-PAYG auth mode`, so
dollars below are list-price equivalents, not a bill.

Scripts: `/tmp/join.py` (builds `/tmp/req.json` from the log), `/tmp/an1.py`
(spend by day and model), `/tmp/an2.py` (output, TTL, duplicates),
`/tmp/an3.py` (sidecar, recache, CCR), `/tmp/comp.py` (transcript composition).

## Where the money goes

Per day, averaged over the four logged days (measured):

| bucket | tokens/day | $/day | share |
|---|---|---|---|
| cache read | 803M | 335 | 62% |
| cache write (1h) | 15.0M | 111 | 21% |
| output | 4.08M | 83 | 15% |
| uncached input | ~1.8M | 10 | 2% |
| hidden CCR rounds (not in ledger) | — | 11 | 2% |

By model: opus 16,733 turns, 134k average context, about $440/day; sonnet-5
8,915 turns, 80k average, about $70/day; fable 1,627 turns on one day; haiku
192 turns.

Composition of what the client sends, from two large Claude Code transcripts
(bytes): thinking 31-35%, tool_result 28-30%, Bash `tool_use` input 22%,
assistant text 4.6-4.8%, user text 3-7%.

## Ideas, ranked by $/day

### 1. Sidecar forwards the 1M-context beta to haiku; 66% of sidecars fall back

**Value:** $10-14/day (estimated from $2.27 in 3.8 hours after the 13:56
restart; $6.91 measured for 09-03). Quality risk: none. The sidecar answers
four words of spinner text.

**Evidence:** `sidecar_detected` 620, `sidecar_fallback` 424, all on 09-03 (the
sidecar is new on this branch). After the restart: 82 detected, 54 fell back,
and every readable error says `The long context beta is not yet available for
this subscription.` The 378 earlier fallbacks logged compressed error bodies
and cannot be read, but the rate matches. The fallback forwards the original
11-message body to sonnet or opus: 30k average context, 48 output tokens.

**Mechanism:** `crates/headroom-proxy/src/sidecar.rs:530-535`, the header copy
loop in `forward()`, passes `anthropic-beta` through verbatim.

**Change:** rebuild `anthropic-beta` for the sidecar request. Split with
`headers::split_beta_tokens` (`headers.rs:47`), drop tokens starting with
`context-1m`, re-join with `merge_beta_tokens` (`headers.rs:64`), omit the
header when nothing remains. Leave every other header alone. Do not pre-empt
other fields; act on whatever the readable errors name after this fix.

**Tests:**
- Unit test beside `rewrite_forwards_only_the_allowlisted_keys`
  (`sidecar.rs:835`): `claude-code-20250219,context-1m-2025-08-07,effort-2025-11-24`
  becomes `claude-code-20250219,effort-2025-11-24`; a value that is only
  `context-1m-...` yields no header.
- Integration, following `tests/integration_beta_header_sticky.rs`: mock
  upstream returns 400 when it sees `context-1m`, 200 otherwise; assert
  `sidecar_detected` without `sidecar_fallback`.

**Live check:** after restart, `grep -c sidecar_detected` against
`grep -c sidecar_fallback` on the current log. Pass: fallback share under 5%
(from 66%), and any remaining `sidecar_fallback.error` is readable and names a
different cause. Rerun `/tmp/an3.py`; the fallback dollar line should approach
zero, and `prefix_composition.model` for sidecar request ids should read
`claude-haiku-4-5-*`.

### 2. Output tokens: instrument the block-type split before touching anything

**Value:** $83/day is the whole output bucket. The part a verbosity instruction
can reach is assistant text, about 7% of output bytes, so the ceiling is near
$5/day even if text halved. The rest is thinking (about 55%) and tool input
(about 35%); cutting those is the quality trade the user rejects. Quality risk
of this plan: none. It only logs.

**Evidence:** transcript composition above. Per assistant turn: thinking about
430 tokens, tool_use about 200, text about 100. Opus output p50 371, mean 645;
86% of output tokens land on `tool_use` turns. The proxy shapes nothing today:
`output_shaper_enabled` defaults off (`config.rs:1197`) and
`~/.headroom-flags.sh` sets `--verbosity-level 2` and `--mechanical-effort low`
but never `--output-shaper`. No thinking-token field appears in the log.

**Mechanism:** the `sse stream closed` event (`proxy.rs:8001-8016`) logs only
`blocks`. `state.blocks` (`sse/anthropic.rs:108`) already holds each block's
`block_type`, `text_buffer` and `partial_json`.

**Change:** add `thinking_chars`, `text_chars`, `tool_input_chars`,
`thinking_blocks`, `text_blocks`, `tool_use_blocks` to `sse stream closed`,
summed over `state.blocks`. Chars, not tokens: the stream carries no per-block
token count and chars/4 ranks well enough. Same fields on the
`stream_incomplete` branch (`proxy.rs:8063`) and on `ccr_continuation_usage`
(`proxy.rs:8091-8101`) so hidden rounds split the same way.

**Tests:** unit test on the summing helper with a `HashMap<usize, BlockState>`
holding one block of each type; assert the three totals. Reuse the
`BlockState` constructors in the `sse::anthropic` tests.

**Check it works:** after two or three days, extend `/tmp/join.py` to read the
new fields and print per model the share of output chars that is thinking,
text and tool input, plus p50/p90 `text_chars` on `end_turn` against
`tool_use` turns. If the live split matches the transcript estimate, the
verbosity lever is capped near $5/day and the question closes with no quality
trade. If text is well above 7%, run one day with
`--output-shaper --verbosity-level 1` (preamble only), compare `text_chars`
per `end_turn` turn against baseline days, and read transcripts for
"explain more" follow-ups. The same fields, joined with
`prefix_composition`, later let a script test whether prior-turn thinking is
billed as cache read, which decides how much of the 31% thinking share in the
prefix is real money.

### 3. CCR accounting: the continuation's cache read is never logged

**Value:** accounting only, but it sets the price of one `headroom_retrieve`.
Hidden rounds cost $11/day measured (913k cache-write tokens and 120k output
tokens per day over 1,461 events) plus an unlogged cache read, estimated at
another $10/day (about 120k tokens per round). Separately, 422 of the 481
turns with more than 5k uncached input tokens are CCR continuations: the
retrieved content, 17k tokens on average and up to 97k, returns as fresh input
at $5/M, about $6/day. Quality risk: none.

**Mechanism:** `ccr_rounds.cache_read_tokens` exists (`proxy.rs:2227`) and is
folded into `RequestOutcome` at `proxy.rs:8112`, but the
`ccr_continuation_usage` event (`proxy.rs:8091-8101`) omits it. The
`turn_cost_ledger` event in `cache_stabilization/usage_observer.rs` sums
rounds into `input_tokens` and `cache_read_input_tokens`, so it disagrees with
`sse stream closed` on 422 of 481 such turns and neither can be reconciled
from the log.

**Change:** add `cache_read_tokens = ccr_rounds.cache_read_tokens` to
`ccr_continuation_usage`. Add `rounds_input_tokens` and
`rounds_cache_read_tokens` as new fields on `turn_cost_ledger`; leave existing
fields as they are.

**Check it works:** for every request id with `ccr_continuation_usage`, assert
`ledger.input_tokens == sse.input_tokens + rounds_input_tokens`. Pass: the
"ledger agrees" count in `/tmp/an3.py`'s companion check goes from 59/481 to
481/481. Then re-price the hidden rounds with the read term included; that is
the number to weigh the offload deferral gate against.

## Measured but not recommended

**tools[] as cache reads: $32/day** (opus $24, sonnet-5 $6.5). 45-58KB and
about 27 tools per turn after `pruned tools[] per policy` removes 3 of 23.
Part of this is the known memory-tool item. Any further cut shrinks what the
model sees, and at 0.1x cache-read pricing it returns little.

**Prior-turn thinking in the prefix: 31-35% of bytes.** `compression/prior_thinking.rs`
already drops it, but only at rebuild boundaries to avoid busting the cache
(205 events, 8.2MB removed over four days). Whether Anthropic bills those
blocks at all is open; plan 2 supplies the data.

## Ruled out, with numbers

- **Proxy response cache** (`--cache true`): only non-streaming bodies qualify
  (`proxy.rs:3266-3284`) and every Claude Code turn streams. Zero
  `semantic_cache_hit` events in 541k lines. Identical bodies (same
  conversation, message count, forwarded bytes, model, within 10 minutes): 13
  in four days, $0.03.
- **Per-turn model routing** (`model_router.rs`, `--extra-model-route`): moving
  one opus turn at 134k context to sonnet costs a 134k sonnet 1h write ($0.54)
  against about $0.08 for the opus cache read plus output. A loss unless the
  whole conversation moves, which is the client's choice.
- **5m instead of 1h TTL:** 26,354 inter-turn gaps under 5 minutes, 422
  between 5 and 60, 21 over 60. Simulated 5m TTL on opus pays $25/day more in
  re-writes than it saves on the 1.25x rate. 1h is right.
- **Retries and failures:** 146 failed turns (429/529, three attempts), zero
  output tokens billed on them.
- **Recache waste:** $9.27/day across all attributions, owned by
  `docs/TODO-recache-classification.md`.
