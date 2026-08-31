# Proxy follow-ups

Four items, investigated 2026-08-23 against `~/headroom-proxy.log`
(~3,200 streams, proxy restarted 16:47). Each entry records what the
evidence says, not what it was assumed to say.

## 1. Volatile-content warning fires mostly on constants

**Status: fixed and verified 2026-08-26.** The suppression described at the
bottom of this item ships in `volatile_detector::emit_one`, keyed on a
`sample_digest` per `(conversation, location)`. Re-measured over the process
that started 2026-08-26 06:54: 368 findings, 254 groups, 221 of them
constant-valued — and **every constant group fired exactly once**. Zero repeat
warnings. What is left is one first-sighting per location, which is the
documented floor: nothing has been observed yet, so nothing can be compared.

**Amended 2026-09-01.** That floor was still too loud, and worse, it was
saying something it could not know. Over the process that started 2026-08-30:
589 warnings, 81 locations seen with more than one sample — and every one of
those samples came from a *single request*, a block holding several dates.
Grouped by `request_id`, not one location was ever observed changing between
turns. So every warning in that window was an unconfirmed first sighting
calling itself `volatile_content_detected`.

First sightings now report at INFO as `volatile_content_suspected`, and WARN
means the value was seen to move. The 2026-08-26 reason for keeping the first
sighting holds — a one-request conversation must still hear something — but it
is answered by the INFO line, not by a warning. The proxy runs at
`--log-level info`, so nothing is lost.

The residue is a `uuid_v4` inside `tool_result` content, and it reads as a new
group each time only because the location carries the message index
(`messages[9]`, `messages[15]`, `messages[21]`). Tightening that further means
keying on something other than position, and it is not worth it at one warning
per site.

120 warnings an hour. The detector flags a value by its *shape* — a
uuid or an ISO timestamp inside the cached prefix — and never checks
whether that value actually changed between turns. A constant uuid in
message 0 costs nothing and is warned about as loudly as a clock that
ticks every request.

Joining findings on `(conversation_key, kind, location)`, which the
`conversation_key` field now makes possible:

| | groups |
|---|---|
| value changed across turns (real cache buster) | 24 |
| value constant (false positive) | 147 |

318 findings, 171 groups, **86% noise**. The worst offender is
`uuid_v4` at `messages[0].content[1].text`: 10 findings, **one**
distinct value, seen across 9 different conversations — a fixed string,
not per-request churn.

The real ones are mostly timestamps: of the 24 changing groups, 16 are
`iso8601_timestamp` and 8 `uuid_v4`.

Fix: keep the last value seen per `(conversation, location)` and warn
only when it differs. Shape alone is not evidence.

## 2. `ccr: dropping blocks the client must not receive`

**Status: fixed 2026-08-23 (`4d48f24d`).** `ccr_stream.rs:818` warns only when
`unresolved_proxy_tool` is non-zero, with the other two counts carried along for
context; the routine case dropped to DEBUG.

215 events. Breakdown by reason:

| reason | blocks |
|---|---|
| `continuation_thinking` | 209 |
| `already_streamed` | 32 |
| `unresolved_proxy_tool` | 2 |

97% is `continuation_thinking` — thinking blocks from a continuation
round that the client must not be shown twice. That is the design
working, logged at WARN. Only `unresolved_proxy_tool` (2 in a day) is
a real fault, and it is the one that kills a turn with "the model's
tool call could not be parsed".

Fix: WARN for `unresolved_proxy_tool`, DEBUG for the other two.

## 3. Build warnings

**Status: mechanical ones cleared (`92a2bb51`); 33 down to 10.** The unused
imports and unused variables are gone, and so is `arrays` in
`content_router.rs:484`. What remains is all dead code rather than mechanical
churn — four never-used functions (`which`, `split_model_action`, `safe_str`,
`record_waste_signals`) and six never-read fields (`agent_type`, `tool_name`,
`session_key`, `max_queue`, `last_mtime_ns`, and `lane`/`url`/
`response_headers` together). Each wants a decision about whether the thing it
belongs to is finished, so none was deleted.

33 warnings, all pre-existing and none from the cache work:

- 15 unused imports (`HashMap` x3, `hex` x2, `Arc`, `HashSet`,
  `HeaderMap`, `MatchedPath`, `chrono::Utc`, `Digest`/`Sha256`,
  `SystemTime`/`UNIX_EPOCH`, `CcrToolCall`)
- 6 unused variables (`tokens_saved`, `state`, `now`, `host_clone`,
  `config`, `client_ip`)
- `arrays` in `content_router.rs:484` assigned twice and never read —
  the only one that might be a real bug
- `NullCcrStore` never constructed

`cargo fix --release --workspace --allow-dirty` clears the mechanical
ones. `arrays` wants a human.

## 4. Mid-stream drops: the connection-pool theory

**Status: suspect confirmed, fixed 2026-08-23 (`f5f2d7ec`).** The client at
`proxy.rs:277` now sets `tcp_keepalive(20s)` alongside the 90s pool idle
timeout. `error decoding response body` by day: 40 on 08-22, 69 on 08-23, then
5, 7 and 11 — roughly an 85% drop, and the remainder no longer tracks time of
day. Not zero, so leave the entry open; it is no longer the largest thing on
this page.

### What the remainder actually is (2026-08-26)

The residual is not the same failure. Splitting the 86 affected requests by the
error's *source* chain rather than its message:

```
cause             08-22  08-23  08-24  08-25  08-26   total
BadRecordMac         16     33      1      0      0      50
BrokenPipe            0      2      0      1      5       8
TimedOut              0      2      0      0      0       2
no cause logged      24     32      4      6      6      72
```

`BadRecordMac` is a TLS record integrity failure, the corruption written up in
`tls-record-corruption-wsl2.md` — a different problem that happened to share a
log line. It stopped on 08-24 and has not recurred. What the keepalive was aimed
at, the idle drop, is gone too: `TimedOut` last fired on 08-23.

What is left is `BrokenPipe`, and it is smaller than the event count suggests.
Eight events are seven requests are **five incidents**: 0, 2, 0, 1, 2 by day,
flat. The three on 08-26 that look like a spike all carry the same timestamp to
the second, on three different request ids — one HTTP/2 connection dying and
taking every stream multiplexed on it, which is one failure, not three.

Every one recovered on retry; one needed two attempts. `held_bytes` is 773-1595
across all of them, so they die within the first couple of KB of body, never
deep into a long stream. Nothing here reached a user.

`BrokenPipe` while *reading* a body is a write that failed: an h2 client has to
send `WINDOW_UPDATE` to keep a large body flowing, and writing that to a socket
the peer has already closed is EPIPE. That fits a connection torn down between
dispatch and first frames, and it is what the pool and the retry exist to
absorb. Five incidents in four days, all absorbed, is not worth a change —
particularly not a speculative one, since the honest gap below means we cannot
yet tell whether the turns that *did* hurt share this cause.

### The instrumentation gap that hid it

Of the 14 requests affected since the fix, 8 ended truncated — and all 8 logged
no cause at all. That is not a coincidence. Only the *retry* site logged
`cause = ?e`; the two sites that mark the give-up — `stream_finisher.rs` and
`proxy.rs` — logged `error = %e`, and `Display` on a `reqwest::Error` is the
bare string `error decoding response body` with the whole source chain thrown
away. The turns that actually reached the user broken were exactly the turns
whose cause was unreadable.

Every site that logs a transport error now logs `cause` too: the two give-up
paths, the two `debug!` sites in `stream_finisher` that were equally blind, and
`vertex/raw_predict.rs`. `stream_retry` already did, which is the only reason
any of the above could be written.

Until traffic runs against that build, every number here is measured on the
recovered half only — the half that hurt nobody. The truncated 8 could be
`BrokenPipe`, could be something else; what was recorded cannot say. That is the
question the next drop answers, and no fix should go in ahead of it.

16 affected requests in ~3,200 streams (0.5%), rate rising through the
day (0 at 15:00, 15 events at 17:00, 10 at 18:00 over 67 streams). All
9 mid-stream failures carry the same error: `error decoding response
body` — a body that ended without its terminating chunk.

Ruled out:

- **Request timeout.** `--upstream-timeout 600s`; the longest drop
  landed at 82s.
- **Stale pooled connection handed out at dispatch.** Every drop logs
  `upstream_status=200` first, so headers arrived and the connection
  was alive. A stale connection fails *before* headers.
- **TTL / idle expiry of the prompt cache.** Unrelated path, but
  checked: all drops sit inside the window.

What is left: the connection dies *mid-body*, 2.4s to 82s in, with no
fixed-timeout signature. The client is built at `proxy.rs:254` with
`pool_idle_timeout(90s)` and **no** `tcp_keepalive`, **no**
`http2_keep_alive_interval`, and HTTP/2 negotiated via ALPN. An SSE
stream that goes quiet between events — a long tool-use pause — has
nothing keeping the flow warm, so any middlebox between here and the
provider (WSL2's NAT included) can drop it silently.

Dropped requests skew larger — median 274KB body / 95k prompt tokens
against 211KB / 75k healthy — but stay inside the normal range
(healthy p90 590KB), so size is a lean, not a cliff.

Next: set `http2_keep_alive_interval` (~20s),
`http2_keep_alive_while_idle(true)` and `tcp_keepalive`, then measure
the drop rate over an hour of traffic.
