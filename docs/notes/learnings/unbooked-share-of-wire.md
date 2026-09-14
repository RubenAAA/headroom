# Learning: unbooked turns are representative-sized

- **Source:** `docs/notes/proxy-experiments-2026-08.md` §9b
- **Claim:** unbooked requests are 17.0% of wire bytes vs 16.1% of requests (near-identical medians) — faster time-to-`forwarded` does not mean smaller. Every booked-only ratio describes 83% of traffic as 100%.


## Item 9b

*moved from `docs/notes/proxy-experiments-2026-08.md`*

### 9b. The two unbooked categories are not the same failure

A live `stream_incomplete` at 21:54:36Z was traced end to end. Its full
lifecycle is present in the log:

```
21:54:34.449  anthropic live-zone dispatch
21:54:34.503  outbound_body_bytes
21:54:35.546  forwarded
21:54:36.544  sse stream closed
21:54:36.544  stream ended without message_stop; usage is partial, not booked
              (partial_input_tokens=2, partial_output_tokens=6)
```

**The SSE task ran to completion and logged.** So the 4 `stream_incomplete`
requests are a *different* failure from the 175 that stop after `forwarded`, and
9a's panic hypothesis applies only to the latter. When the state machine runs it
leaves a trace; the 175 leave none, which is what makes "the task never got
there" the right shape of explanation for them.

It also looked at first like a reason to downgrade items 6 and 9: this unbooked
turn cost 2 input and 6 output tokens, and the silent requests reach `forwarded`
faster than booked ones (0.68s vs 1.66s median), suggesting they might all be
small. **Measured, and they are not.**

Joining `anthropic live-zone dispatch` against PERF on request id, and sizing
each by `bytes_out` from `outbound_body_bytes`:

```
dispatched 2524   booked 2118   unbooked 406 (16.1%)
booked    n=2118  bytes_out 631,542,327  median 289,834
unbooked  n= 406  bytes_out 129,326,273  median 279,509
```

Unbooked requests are **17.0% of all wire bytes sent upstream** against 16.1% of
requests — very slightly *larger* than average, with a near-identical median.
The faster time-to-`forwarded` does not mean smaller bodies.

So roughly a sixth of everything the proxy sends upstream is missing from cost
and savings accounting, and it is a representative sixth, not a tail of trivial
calls. Every ratio in this document computed over booked requests — item 2's
totals, item 3's waste-versus-savings comparison, item 10's pricing — is drawn
from 83% of the traffic while describing 100% of it.

Reproduce with the join above; `bytes_out` is the post-transform size actually
put on the wire, which is the right basis for cost. The 2-token example is a
real member of this set, just not a typical one.
