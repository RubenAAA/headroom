# Implemented: SSE blind spots closed

- **Status:** closed 2026-08-12 (items 9/9a/18 + waiter; residual rate open — see file)
- **Source:** `docs/notes/proxy-experiments-2026-08.md` §§9, 18


## Telemetry settlement

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **9 — not reproducing on the new binary.** 35 dispatched, 35 `sse stream
  closed`, 35 booked, **0** task failures, **0** missed chunks, no gap. The
  12–16% blind spot did not appear in this window. That is not proof it is gone
  — the original was bursty (33% in one hour, 4% in another) and 35 turns is a
  small sample — but there is no live failure to diagnose, so there is nothing
  to fix yet. The instrumentation is armed and will name the cause when it
  recurs.


## Fix 9 waiter

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **9:** the detached SSE parser task is now awaited by a second detached waiter.
  Panics and cancellations log a request-scoped error instead of disappearing
  with a dropped `JoinHandle`. The waiter also logs normal completion with the
  number of chunks sent to the parser and chunks dropped because its queue was
  full or closed. This does not yet recover accounting for a task that fails;
  it distinguishes parser failure from upstream/body lifecycle loss so the
  remaining cause can be measured.


## Fix 9 chunk counts

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **9:** SSE parser completion and failure events include sent and dropped chunk
  counts, allowing queue pressure and parser failure to be separated from an
  upstream/body lifecycle gap.


## Item 9

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Item 9a

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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

**Closed 2026-09-02.** The parser task now keeps its `JoinHandle` and a waiter
awaits it, at `crates/headroom-proxy/src/proxy.rs:5102` — "do not drop its
JoinHandle: a panic would otherwise erase the only completion record for this
request." Re-measured on `~/headroom-proxy.log` over the 09-01 binary, from
07:55:30Z on 09-01 to 12:22Z on 09-02: 154 of 9,625 forwarded requests reach no
completion record, or 1.6%, against 11.9% above. The residue is spread evenly
across hours rather than bunched, so it no longer looks like one failure mode
and no longer hides a large part of the bill. An earlier pass the same morning
read 43 of 8,763, or 0.5%; the two disagree by more than the extra traffic
explains, and neither pass kept the request ids, so the true residual rate is
somewhere between them and worth one clean re-run before anyone quotes it.


## Item 18

*moved from `docs/notes/proxy-experiments-2026-08.md`*

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


## Closure 9

*moved from `docs/notes/proxy-experiments-closures.md`*

**9 — closed 2026-08-12: no current dispatched request disappears silently.**
Item 18 already identified the historical gap as provider rejections: a 400 is
not an event stream, so the SSE parser cannot emit a completion. The current
proxy buffers and logs those bodies as `upstream_rejected`. It also retains the
detached SSE parser's `JoinHandle` in a waiter that logs panic/cancellation and
tracks chunks dropped from the telemetry queue, closing item 9a's structural
blind spot even though no parser panic was needed to explain the old sample.

The current traffic reconciles completely. From 12:52:48Z to the 14:26:29Z
restart, 413 live-zone dispatches split into 412 `sse stream closed` + PERF
bookings and one fully instrumented 429 `upstream_rejected`; zero dispatches are
unclassified. From the restart through a 14:35:45Z cutoff, all 56 dispatches
have both close and PERF records. There are zero `sse state-machine task failed`
events and zero completions with missed parser chunks in either window. Two
additional post-restart `forwarded` records are `/v1/messages/count_tokens`,
which intentionally have no PERF outcome and must not be included in the SSE
denominator. A request dispatched after the cutoff was still in flight when the
query ran and is likewise not misclassified as missing. The old 11.9% silent
category is therefore 0 of 469 completed/currently-classifiable dispatches.
