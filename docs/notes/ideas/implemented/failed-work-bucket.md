# Implemented: failed turns get separate durable books

- **Status:** closed 2026-08-12
- **Source:** `docs/notes/proxy-experiments-closures.md` (item 6)
- **Summary:** failed turns skipped all sinks (no inflation) but left no trace.
  `record_failed` now writes schema-v4 `failed_work` (requests, attempts,
  forwarded + at-risk tokens, status counts, optional provider usage),
  persisted under `/stats.persistent_savings.failed_work`; success totals
  untouched. 4xx still excluded from the bucket (terminal 5xx only).


## Fix 6

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Item 6

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Closure 6

*moved from `docs/notes/proxy-experiments-closures.md`*

**6 — closed 2026-08-12: failed turns have separate durable books.** The early
return in `emit_request_outcome` remains: a terminal 5xx cannot improve the
successful savings rate, cost totals or PERF population. `record_failed` now
writes a schema-v4 top-level `failed_work` aggregate instead. It records failed
requests, upstream attempts, one-body forwarded tokens, forwarded tokens at
risk across all attempts, status counts, and optional provider-reported input
and output usage. Provider usage is deliberately separate from the request-side
estimate rather than fabricated when a rejection has no usage block. The
aggregate is persisted and exposed under
`/stats.persistent_savings.failed_work`; lifetime, session and project success
totals are untouched.

The controlled before/after uses a 529 upstream and three configured attempts.
Before the wiring, all three upstream requests occurred but the successful
lifetime remained zero and there was no failed-work record; the regression test
failed on `failed_work.requests == 0`. It also exposed a structural gap: the
small non-SSE rejection branch logged `upstream_rejected` but never constructed
a `RequestOutcome`. After the fix, the same run records one failed request,
three upstream attempts, one forwarded-body estimate, and
`forwarded_tokens_at_risk == 3 * forwarded_tokens`; provider usage observed is
zero and all successful request/token totals remain zero. A tracker persistence
test separately pins two failures across statuses 529 and 503, five total
attempts, 51,000 forwarded tokens, 143,000 at risk, and optional actual usage
on only one request.
