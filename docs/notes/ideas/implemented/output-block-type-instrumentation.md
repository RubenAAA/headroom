# Idea: instrument the output block-type split before touching anything

- **Status:** implemented 2026-09-11 (09-11 re-audit)
- **Source:** `docs/notes/savings-ideas-1.md` §2 (2026-09-03 log window)
- **Summary:** the $83/day output bucket can't be reasoned about without knowing
  its split. Transcript estimate: thinking ~55%, tool input ~35%, assistant
  text ~7%. A verbosity lever can only reach the text — ceiling near $5/day
  even if text halved. Add `thinking_chars`, `text_chars`, `tool_input_chars`
  (+ block counts) to `sse stream closed`, `stream_incomplete`, and
  `ccr_continuation_usage`, summed over `state.blocks` (`sse/anthropic.rs:108`).
- **Value:** decides the verbosity question with data; same fields later test
  whether prior-turn thinking bills as cache read (decides the 31% prefix
  thinking share in `savings-ideas-1.md` "measured but not recommended").
> **09-11 outcome:** logging shipped; shaper trial declined — no trial, no quality trade.
- **Next (superseded):** unit test on the summing helper; after 2–3 days extend the join
  script for per-model shares and p50/p90 `text_chars` on `end_turn` vs
  `tool_use` turns. If text ≫ 7%, trial `--output-shaper --verbosity-level 1`
  for a day and read transcripts for "explain more" follow-ups.


## Detail

*moved from `docs/notes/savings-ideas-1.md`*

### 2. Output tokens: instrument the block-type split before touching anything

> **CLOSED (2026-09-11) — logging shipped, decision: do not implement.**
> The split below shipped (`OutputSplit` in
> `crates/headroom-proxy/src/sse/anthropic.rs:99-122`, logged on
> `sse stream closed` at `proxy.rs:9402-9408`). Live check on
> `~/headroom-proxy.log` (2,586 closes, 2026-09-10 13:06–23:20 UTC):
> visible chars text **32%** / tool input **68%** / thinking **0%** —
> thinking is redacted on the wire, not absent (1,347 turns carry
> `thinking_blocks`, 5,875 `thinking_deltas`, all zero chars; billed
> output 1.30M tokens runs ~2.9x visible chars). Scaled to 24h, text is
> ~1.13M chars/day ≈ ~280k tokens ≈ **7–9% of output**, matching the
> transcript estimate. `end_turn` (209) `text_chars` p50 **278** / p90
> **2740**; `tool_use` (2,373) p50 **0** / p90 **118**. The verbosity
> lever stays capped near **$5/day**, so the question closes with no
> quality trade and no `--output-shaper` trial. Kept for the
> measurement record.

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
