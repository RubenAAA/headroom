# Idea: log the CCR continuation's cache read

- **Status:** implemented 2026-09-11 (09-11 re-audit)
- **Source:** `docs/notes/savings-ideas-1.md` §3 (2026-09-03 log window)
- **Summary:** `ccr_rounds.cache_read_tokens` exists and folds into
  `RequestOutcome`, but the `ccr_continuation_usage` event omits it — so the
  price of one `headroom_retrieve` is unpriced (~$10/day estimated on top of
  $11/day measured hidden-round cost). Add `cache_read_tokens` to
  `ccr_continuation_usage` plus `rounds_input_tokens` /
  `rounds_cache_read_tokens` on `turn_cost_ledger` (existing fields untouched).
- **Value:** accounting-only, but it sets the number to weigh the offload
  deferral gate against; also reconciles the 422/481 ledger-vs-SSE disagreements.
> **09-11 outcome:** both halves shipped; price is ~$0.026/retrieve (~$13/day).
- **Next (superseded):** assert `ledger.input_tokens == sse.input_tokens + rounds_input_tokens`
  for every request id with `ccr_continuation_usage` (59/481 agree today → 481/481),
  then re-price hidden rounds with the read term.


## Detail

*moved from `docs/notes/savings-ideas-1.md`*

### 3. CCR accounting: the continuation's cache read is never logged

> **DONE (2026-09-11).** Both halves shipped: `cache_read_tokens` on
> `ccr_continuation_usage` (`proxy.rs:9530`, with `client_cache_read_tokens`
> beside it) and now `rounds_input_tokens` / `rounds_cache_read_tokens` on
> `turn_cost_ledger` (billed totals minus the client baseline, zero on the
> single-round path; existing fields untouched). Unit test
> `the_cost_ledger_bills_continuation_rounds` pins the split (10 / 150000
> on the fixture). Live check on `~/headroom-proxy.log` (169 CCR turns,
> 2026-09-10): all 169 reconcile as `ledger == sse + rounds`, and the
> priced answer is ~68.6k read + 2.1k write + ~300 output per round ≈
> **~$0.026/retrieve** at sonnet-5 rates (~$13/day) — the number to weigh
> the offload deferral gate against. Kept for the measurement record.

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
