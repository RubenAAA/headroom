# Implemented: over-cap Retry-After returns instead of retrying early

- **Status:** closed 2026-08-12 (both direct + routed paths)
- **Source:** `docs/notes/proxy-experiments-closures.md` (item 14)
- **Summary:** a 31 s `Retry-After` against a 30 s cap retried after 30 s —
  non-compliant. Both paths now parse uncapped; over-cap returns the original
  status/body/`Retry-After` so the client schedules compliantly, with named
  over-cap events. Within-cap headers + backoff retry normally.


## Fix 7a

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **7a:** routed-model retries now carry request IDs and the same header/source/
  clamp fields as the main proxy retry path, including transport-backoff source.


## Fix 14

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **14:** the Anthropic and routed retry warnings carry `session_key_hash`, so a
  retry burst can be joined to the recache events that follow it on the same
  session. That tests the ordering item 14 asserts; it does not assume the clamp
  caused the cold cache. Note the join reaches the *drift* events cleanly and
  the *recache* events only through time, until the gap above is closed.


## Item 7a

*moved from `docs/notes/proxy-experiments-2026-08.md`*

### 7a. There is a second retry path, and the fix does not cover it

Found live at 22:56:43Z when the codex upstream returned 503 — the first
non-429/529 retry of the run:

```
22:56:43  event=local_model_upstream_retry  status=503  attempt=1  backoff_ms=1000
22:56:44  event=local_model_upstream_retry  status=503  attempt=2  backoff_ms=2000
```

`handlers/local_model.rs:1365-1389` retries independently of the `proxy.rs`
loop, with its own header parsing. Three differences that matter:

1. **It has none of the new fields.** The item 7 fix added
   `retry_after_header`, `delay_source` and `retry_after_clamped` to
   `proxy.rs:3582` only. This path still logs just status, attempt and
   `backoff_ms`, so the blind spot item 7 exists to close is still open on
   every request routed through a model route.
2. **No RFC 2822 fallback.** It parses numeric seconds only
   (`:1366-1370`); `proxy.rs:3554` also accepts an HTTP-date. A date-format
   `Retry-After` is silently ignored here and falls through to backoff.
3. **No jitter** (`:1373-1377` — plain `base * 2^(attempt-1)`). So **the
   field test in item 7 above does not apply to this path.** Round values like
   1000/2000 are ordinary backoff here, not evidence of a header. Reading
   `local_model_upstream_retry` lines with the proxy.rs table would get the
   diagnosis backwards.

It also clamps to `max_delay_ms` (`:1378`), so item 7's clamp decision applies
to both paths and should be made once.

The warn carries no `request_id`, so these retries cannot be joined to the turn
they belong to — same gap as `model_route_translate` in item 15.

**Fix:** add `retry_after_header`, `delay_source` (`header` | `backoff`) and a
`clamped` flag to the warn at `proxy.rs:3579`. One line, no behaviour change,
and it makes all three behaviours answerable. Note `proxy_log_audit.py retries`
reports `retry_after seen anywhere: 0` — that is the missing log field, not
missing behaviour.

**Separately, decide the clamp.** `retry_max_delay_ms` (30s) silently overrides
a longer server instruction. Honouring a 60s `retry-after` means holding the
request 60s; clamping means a near-certain wasted retry. Under a session limit,
which is when this fires, the server value is the only one that can be right.
See item 14: the same burst was followed by three full prefix rewrites costing
326K tokens, so the clamp may be buying a faster retry at the price of a cold
cache.


## Item 14

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 14. Rate limiting costs cache, not just time

Observed live at 22:19-22:21Z, immediately after the 429 burst in item 7. Over
22:00-22:29Z there were 120 turns on conversations of 40+ messages: **117 warm,
3 cold — and all 3 cold starts fall in the two minutes after the rate limit
cleared.**

```
22:16:46      msgs=149  cache_read= 22,032  cache_write= 76,034
              <- 429 burst, ~2.5 min gap, no traffic ->
22:19:05 COLD msgs= 95   cache_read=      0  cache_write= 95,933
22:19:14      msgs= 97   cache_read= 95,933  cache_write=  1,388
22:19:21 COLD msgs=152   cache_read=      0  cache_write=101,564
22:19:30      msgs=155   cache_read=101,564  cache_write=  1,540
22:21:33 COLD msgs= 65   cache_read=      0  cache_write=128,514
```

Each cold turn rewrites the whole prefix, and the very next turn on that
conversation reads back exactly what it wrote (95,933 → 95,933; 101,564 →
101,564). The cache works — it had simply gone.

**The gap is under the 5-minute TTL.** 22:16:46 to 22:19:05 is 2m19s, so TTL
expiry alone does not explain it. Candidates, in order of likelihood:

1. The retries themselves. Three requests exhausted their attempts against a
   rate limit; if a rejected request still counts as a cache write attempt
   upstream, or if the clamped 30s retry landed while the limit was open, the
   prefix may have been evicted rather than expired.
2. The clamp in item 7 — retrying into a closed window churns the prefix.
3. Ordinary eviction under concurrency, unrelated to the limit.

**Cost.** 326,011 tokens of `cache_write` across the three, against ~1,400 on a
normal warm turn. Being rate-limited is not just a delay; it bills a full
prefix rewrite per affected conversation once traffic resumes.

Worth checking against item 7's fix: if honouring a longer `retry-after`
prevents the wasted retries, it may also prevent this. That would make the
clamp decision worth more than the delay it saves.


## Closure 14

*moved from `docs/notes/proxy-experiments-closures.md`*

**14 — closed 2026-08-12: an over-cap `Retry-After` no longer causes an early
retry.** The live audit still provides useful negative evidence: five exhausted
429 requests and ten status retries all used backoff, not a Retry-After header,
and zero warmed conversations went cold in their post-limit windows. The risky
branch was therefore measured with a controlled upstream: `Retry-After: 31`, a
30-second internal cap and three configured attempts. The former policy parsed
the instruction through a capped helper, making 31 seconds indistinguishable
from permission to retry after 30.

Both direct Anthropic and routed Responses paths now parse the uncapped value.
When it exceeds the maximum in-request wait, they do not retry early: they
immediately return the original upstream status, body and `Retry-After` header
so the client can schedule a compliant later request. The controlled after run
records exactly one upstream request on each path and receives 429 with header
`31`; the routed streaming request also remains 429 instead of being translated
into a 200 SSE response. Header delays within the cap and ordinary exponential
backoff continue to retry normally. Dedicated over-cap events name the request,
attempt and rejected delay.
