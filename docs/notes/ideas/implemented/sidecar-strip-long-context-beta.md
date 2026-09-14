# Implemented: strip context-1m beta from sidecar requests

- **Status:** implemented 2026-09-10 (`strip_long_context_beta`,
  `crates/headroom-proxy/src/sidecar.rs:524-536`)
- **Source:** `docs/notes/savings-ideas-1.md` §1, `docs/notes/savings-ideas-2.md` §4.3
- **Summary:** the spinner sidecar forwarded the `context-1m*` beta to haiku,
  which subscription can't serve — 66% fell back, each fallback forwarding the
  full 11-message body to sonnet/opus ($7–14/day) and poisoning the main
  session's cache (drift + dropped replay store). Fix rebuilds `anthropic-beta`
  minus `context-1m*` tokens.
- **Verify:** `sidecar_fallback` share < 5%; fallback dollar line → ~0.


## Detail

*moved from `docs/notes/savings-ideas-1.md`*

### 1. Sidecar forwards the 1M-context beta to haiku; 66% of sidecars fall back

> **DONE (2026-09-10).** The fix below shipped: `strip_long_context_beta`
> in `crates/headroom-proxy/src/sidecar.rs:524-536` drops `context-1m*`
> tokens (unit tests beside it use the doc's exact example values).
> Kept for the measurement record; re-run the live check to confirm the
> fallback share fell.

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


## Detail

*moved from `docs/notes/savings-ideas-2.md`*

### 4.3 Spinner-text sidecar falls back two times in three

> **DONE (2026-09-10)** — `strip_long_context_beta`
> (`sidecar.rs:524-536`) strips `context-1m` as proposed below. Kept
> for the measurement record.

`sidecar_detected` 557 in AM, 378 fell back; live 99 detected, 62 fell
back, every one with status 400 "The long context beta is not yet available
for this subscription" (`sidecar.rs:388` passes the client's beta header to
haiku). A fallback forwards the original request in full, under a different
system prompt. Ledger cost of fallback turns: AM 182 turns, 627,133 write
tokens (6.8% of the file), median read 48k; live 29 turns, 100,355.

The different system prompt is a drift: 17 of 50 `system` drift events and
16 of 30 `early_messages` drift events in AM are the sidecar request
itself, and each drift drops the replay store
(`prefix_replay_invalidated_on_rebuild` 43/1000 turns AM vs 1/1000
baseline; later turns carrying it: AM n=95, 92.6% shortfall, 590k re-write).

What to do: strip `context-1m` from the sidecar's beta header. Then the
answer is a haiku call on 2–4 messages and never touches the main session's
cache. In AM the fallback error body is logged as undecoded compressed
bytes; live logs it decoded.


## 09-11 verdict

*moved from `docs/notes/savings-ideas-2.md`*

- **4.3 — done, confirmed live.** `strip_long_context_beta`
  (`sidecar.rs:524`, tested `:877`) holds: 281 `sidecar_detected`, zero
  status-400 fallbacks on the beta message.
