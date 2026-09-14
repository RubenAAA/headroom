# Learning: an absent log field is absent evidence, not absent behavior

- **Source:** `docs/notes/proxy-experiments-2026-08.md` ("one item was wrong",
  2026-08-09)
- **Claim:** item 7 said the proxy ignores `Retry-After`; it reads and honors
  it (`proxy.rs:3543-3564` at the time) — the item had mistaken a missing log
  field for missing behavior, and became a one-line log fix.
- **Evidence:** same trap behind items 4-caveat, 5, and the 9/9a split.
- **Rule:** anything resting mainly on "the log never says X" gets a source
  check before anyone acts on it.


## Fix 7 fields

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **7:** retry warnings now include `retry_after_header`, `delay_source`, and
  `retry_after_clamped`, making header use and the 30-second clamp observable
  without changing retry behavior. The clamp flag is based on parsed header use
  (`delay_source == "header"`), not header presence, so an unparseable header
  cannot be mislabeled as the source of a capped backoff.


## Item 7

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 7. 429 retries don't *log* `Retry-After` (corrected)

**This item said "429 retries ignore `Retry-After`". That was wrong.** The
header is read and honoured. Correction made after reading the source; the
original claim rested on the header never appearing in the log, which measures
log coverage, not behaviour.

`proxy.rs:3543-3564` parses `retry-after` as numeric seconds, then falls back to
an RFC 2822 date, and clamps to `retry_max_delay_ms`. Line 3565 uses it as
`delay_ms`, dropping to jittered exponential backoff (`retry_base_delay_ms`,
1000ms, `config.rs:1963`) only when the header is absent or unparseable.

The real gap is narrow: the warning at `proxy.rs:3579-3586` logs `attempt`,
`status`, `max_attempts` and the final `delay_ms`, but never whether the header
was present or used. So the log cannot distinguish "server said 500ms" from
"server said nothing and backoff computed 500ms".

Observed delays:

```
429:        500, 620, 830, 980, 1140, 1660, 2720 ms
overloaded: 1000 x6, 2000 x2 ms
```

The 620/830/1140/2720 values carry jitter, so those came from local backoff. The
flat 500 is suspicious: base is 1000ms and jitter spans 50-150%, so a first
attempt cannot produce 500ms — the minimum is 500ms exactly, at jitter=50. Not
conclusive.

**A second window answers it without touching the code.** All 429s on the
evening of 2026-08-08 (UTC):

```
21:10:38  attempt=1  delay_ms=1010     21:10:39  attempt=2  delay_ms=2480
21:22:34  attempt=1  delay_ms=1350     21:22:36  attempt=2  delay_ms=1320
21:38:13  attempt=1  delay_ms= 780     21:38:15  attempt=2  delay_ms=1180
22:06:21  attempt=1  delay_ms= 960     22:06:22  attempt=2  delay_ms=2140
```

The header path returns `secs * 1000`, so honouring it yields a round multiple
of 1000. **Not one of these is round**, and every attempt=1 value sits inside
[500,1500] — exactly `base=1000` under 50-150% jitter. Local backoff computed
all eight, so `retry-after` was absent or unparseable on every one.

This does not weaken the fix. The header is honoured *when sent*; upstream
simply is not sending it here, which is a fact about Anthropic's 429s worth
knowing before anyone builds rate-limit handling on the assumption it arrives.
The missing log field is what forced this roundabout inference from jitter
arithmetic.

**Superseded 40 minutes later — the header does arrive, under real rate
limits.** A genuine session-limit burst at 22:16-22:18Z:

```
22:16:38  attempt=1  delay=30000  rid=3387a58b
22:16:47  attempt=1  delay=30000  rid=de84ef82
22:17:10  attempt=2  delay=30000  rid=3387a58b
22:17:20  attempt=2  delay=30000  rid=de84ef82
22:17:53  attempt=1  delay=30000  rid=8c04349a
22:18:26  attempt=2  delay=30000  rid=8c04349a
```

Every value is exactly 30000, which is `retry_max_delay_ms`
(`config.rs:1964`). Backoff cannot produce it: attempt index 0 gives
`1000 × 1` jittered to 500-1500, index 1 gives `1000 × 2` jittered to
1000-3000. Landing on 30000 exactly, six times, requires the header path —
`(secs * 1000).min(max_delay)` with `secs >= 30`.

So the earlier reading was drawn from the wrong sample. The 21:xx 429s were
incidental and carried no header; a real rate limit sends `retry-after` with a
long value, and the proxy honours it and clamps to 30s. **The clamp is now the
question worth asking:** if upstream says wait 60s and the proxy waits 30s, it
retries into a still-closed window and burns an attempt. Both retries here were
clamped, and all three requests exhausted their attempts.

Two lessons for this document. The header is read, honoured, and clamped —
three separate behaviours, none of them visible in the log, which is exactly why
this item flipped twice. And a quiet window is not a representative sample:
rate-limit behaviour can only be observed while being rate-limited.

**`delay_ms` distinguishes the two kinds of 429 on sight.** A later burst at
22:30:25Z read `delay=1280` then `delay=2700` — jittered, so local backoff, so
no header. Compare the 22:16Z session limit, six flat 30000s. Until the log
field lands, this is the field test:

| `delay_ms` | means |
| --- | --- |
| exactly 30000 | header present, value >= 30s, clamped — a real limit |
| jittered, in [500,3000] | no usable header — an incidental 429 |

Useful when reading old logs, and it means the two kinds should never be pooled
into one retry statistic.
