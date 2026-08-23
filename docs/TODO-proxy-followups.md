# Proxy follow-ups

Four items, investigated 2026-08-23 against `~/headroom-proxy.log`
(~3,200 streams, proxy restarted 16:47). Each entry records what the
evidence says, not what it was assumed to say.

## 1. Volatile-content warning fires mostly on constants

**Status: cause found, fix not written.**

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

**Status: benign, warning level is wrong.**

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

**Status: counted, untouched.**

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

**Status: partly ruled out, better suspect found.**

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
