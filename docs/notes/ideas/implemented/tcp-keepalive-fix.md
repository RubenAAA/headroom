# Implemented: TCP keepalive against mid-stream idle drops

- **Status:** fixed 2026-08-23 (`f5f2d7ec`); ~85% drop in `error decoding
  response body` (40–69/day → 5–11)
- **Source:** `docs/notes/proxy-followups.md` §4
- **Summary:** pooled client had 90 s idle timeout with no keepalive; quiet SSE
  gaps let middleboxes (WSL2 NAT included) drop connections mid-body.
  `tcp_keepalive(20s)` alongside. Remainder re-triaged: `BadRecordMac` (TLS
  corruption, separate doc, stopped 08-24), `BrokenPipe` (5 absorbed incidents
  in 4 days — pool+retry working, not worth a change), plus a transport-error
  `cause` logging gap closed at every give-up site.


## Detail

*moved from `docs/notes/proxy-followups.md`*

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

**Closed 2026-09-11 — the next drops answered.** All 17 `error decoding
response body` events in the window carry `cause`: 16 × ConnectionReset-class
Decode, 2 × Request. 17 events over 2,586 streams (0.66%), held bytes 1–5 KB
(dies at first frames, as before). Only 1 `routed_stream_aborted`
(post-commit, user-facing) against those 17 — the retry path absorbs the
rest; user-facing rate is ~1 abort per 10 h. Residual is environmental peer
resets, not a proxy defect. Not worth further work; the remaining question
in this entry is answered.

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
