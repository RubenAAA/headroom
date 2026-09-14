# Error Handling

!!! note "Live implementation: Rust"
    The production proxy is the Rust binary (`crates/headroom-proxy`, launched with `cclaude`). This page describes the Rust error surface operators actually encounter: log event names, HTTP statuses, and what shows up on the statusline endpoints. The old Python exception table was cut — `HeadroomError` and friends exist only in the read-only `upstream-python/headroom/exceptions.py` mirror, and no live code raises them (there is no `headroom/` package outside `upstream-python/`).

Every event name and endpoint below was verified against the Rust sources with `grep`. The log is JSON lines at `~/headroom-proxy.log`; the live endpoints are `GET /healthz`, `GET /cache-health`, `GET /metrics`, and `GET /stats` (see `crates/headroom-proxy/src/proxy.rs`).

## Where errors surface

An error in the proxy reaches you through up to three channels:

1. **The log** — a structured line in `~/headroom-proxy.log`, greppable by its `event` field.
2. **The client response** — an HTTP status (plus `Retry-After` / `x-headroom-*` headers where retry matters).
3. **The endpoints** — counters on `/cache-health` (usage snapshot + upstream verdict), `/stats` (`lifetime_metrics`, including `failed_work`), and `/metrics` (Prometheus).

## ProxyError → HTTP status

`crates/headroom-proxy/src/error.rs` maps the `ProxyError` enum to responses. The log line for all of these carries `event = "proxy error"`.

| Variant | Client sees | Meaning |
|---|---|---|
| `Upstream` (timeout / connect / request / body) | **503** + `Retry-After: 2` + `x-headroom-retryable: transport-exhausted` | Transient transport failure (VPN rotation RST, wifi flap, corpse-pool first-write miss, connect timeout). 503 so stock client retry policies fire instead of stalling the agent loop. |
| `Upstream` (decode / builder / non-retryable) | **502**, no `Retry-After` | Retrying cannot succeed. |
| `InvalidUpstream`, `WebSocket` | **502** | Bad upstream URL or websocket failure. |
| `InvalidHeader` | **400** | Bad request header. |
| `PayloadTooLarge` | **413** | Request body exceeded the configured cap. (Previously mis-surfaced as 400; clients with retry-on-413 logic depend on the 413.) |
| `Io`, `CompressionStartup`, `Config` | **500** | `CompressionStartup` / `Config` are fatal at startup — if compression is configured but the engine will not build, the operator learns at launch, not at the first LLM request. |

## Upstream retries (429 / 529 / 5xx)

The forward path retries transient statuses — **429, 529, and 5xx** (`crates/headroom-proxy/src/proxy.rs`, `send_with_retry` in `crates/headroom-proxy/src/sidecar.rs`, and the routed path in `crates/headroom-proxy/src/routed/retry.rs`).

- `Retry-After` is honored via `headroom_core::retry::retry_after_ms_uncapped`. When the header asks for longer than the internal wait cap, there is **no early retry** — the response is returned as-is and this is logged:
    - `event = "upstream_retry_after_exceeds_cap"` on the Anthropic path (with `status`, `attempt`, `retry_after_ms`, `retry_max_delay_ms`).
    - `event = "local_model_retry_after_exceeds_cap"` on the routed path.
- Each actual re-send is logged as `upstream returned retryable status; retrying` (message text, no `event` field — grep the message, not `event=`) and counted via `record_upstream_retry` by reason (`status_429`, `status_529`, `status_5xx`, transport).

!!! note "429 is not a proxy fault"
    The upstream-health window (`crates/headroom-proxy/src/observability/upstream_health.rs`) counts 4xx-other-than-429 as "we sent something unusable" but explicitly excludes 429 — a 429 means the provider throttled a healthy setup. The verdict is visible as `upstream` on `GET /cache-health`.

## Concurrency shed (proxy-side 429)

When `--max-conversation-concurrency` trips, the excess turn of a conversation is shed with **429 + `Retry-After: 1`** and an Anthropic-shaped `rate_limit_error` body, so the client's standard rate-limit retry fires and the retry lands against a committed prefix (`conversation_concurrency_shed_response` in `crates/headroom-proxy/src/proxy.rs`).

- Log: `event = "conversation_concurrency_shed"`.
- Headers: `x-headroom-shed: conversation-concurrency` — this distinguishes proxy pacing from provider throttling on dashboards. Do not confuse the two.
- Counters: `headroom_conversation_concurrency_sheds_total` on `/metrics`, and `concurrency_sheds_total` in the `GET /cache-health` snapshot.

## Stream defects

The SSE state machines (`crates/headroom-proxy/src/sse/anthropic.rs`, `openai_chat.rs`, `openai_responses.rs`, `framing.rs`) define `thiserror` enums for malformed wire data. Per project rules these **never silently degrade and never panic** — callers `tracing::warn!` and either drop the event or close the stream.

`StateError` variants by surface:

- Anthropic and OpenAI Responses: `PayloadNotUtf8`, `PayloadNotJson`, `MissingField`.
- OpenAI Chat Completions: `PayloadNotUtf8`, `PayloadNotJson` only.
- Framing: `FramingError::EventNameNotUtf8` (binary in the `event:` line — real providers never emit this).

What you will actually see in the log:

- `event = "sse_unknown_event"` — an event with a missing `event:` line (Anthropic/Responses always send one) or an unexpected one (Chat Completions never uses `event:`). Dropped, with `provider`, `event_name`, and a payload preview.
- `event = "sse_partial_json_unparseable"` — accumulated `input_json_delta` fragments did not parse at `content_block_stop`. Non-fatal: the raw fragment is kept in `BlockState.partial_json` for replay/telemetry.
- `event = "tool_call_defect"` with `kind` one of `missing` / `unterminated` / `unparseable`, plus `stop_reason`, `output_tokens`, and a human-readable `detail`:
    - `missing`: `stop_reason` was `tool_use` but no tool block ever started.
    - `unterminated`: a tool block started and never got its `content_block_stop` (`index`, `name`, buffered bytes).
    - `unparseable`: a completed tool block whose accumulated input is not JSON (`index`, `name`, serde error, bounded excerpt).

!!! warning "A defective tool call looks clean on the proxy side"
    When `tool_call_defect` fires, the client rejects the whole turn ("the model's tool call could not be parsed") while the proxy logs an ordinary `sse stream closed` info line (message text, no `event` field). If the far side reports an unparseable tool call and your side looks clean, grep for `tool_call_defect` — that is the trace.

Truncated streams (no `message_stop`: a 429, a dropped stream, a client hangup) increment `proxy_stream_incomplete_total` on `/metrics` via `record_stream_incomplete`. A turn cut short by an upstream **5xx** is failed work, not a missing turn: it goes to `record_failed`, visible as `failed_work` (with `by_status` breakdown) in `lifetime_metrics` on `GET /stats`.

## Sidecar fallback

The spinner-text sidecar (`crates/headroom-proxy/src/sidecar.rs`) retries its shrunk request on 429/529/5xx and transport errors (`event = "sidecar_upstream_retry"`, per attempt). If the shrunk request still fails — transport error, non-2xx, serialize/build failure — it **falls back to the normal pipeline instead of surfacing**:

- Log: `event = "sidecar_fallback"` with `request_id`, upstream `status`, and `error` detail.
- Counter: `proxy_sidecar_total{kind="fallback"}` on `/metrics`.
- Client impact: none by design. A 502 here would be a dead spinner the untouched proxy would have filled, so falling back costs one wasted upstream call and lands on exactly today's behavior. The sidecar never appears on `/cache-health` or the statusline.

## Shutdown

`crates/headroom-proxy/src/main.rs` logs one of two lines at exit:

- `event = "shutdown_drained"` (info) — in-flight requests finished; carries `drain_ms`.
- `event = "shutdown_drain_timed_out"` (warn) — the grace period expired with requests still in flight; carries `timeout_s` and `drain_ms`. A drain that overruns while a client is still streaming is expected; one that overruns with nothing in flight is a task that never sees the shutdown.

Neither appears on any endpoint — the process is exiting.

## Safety guarantees

The old page's core principle still holds, in Rust form:

1. **The sidecar never makes a turn worse.** Every sidecar failure falls back to the untouched request.
2. **Transient failures are retryable by construction.** Transport exhaustion answers 503 + `Retry-After`, and the concurrency shed answers 429 + `Retry-After`, so stock client retry policies do the right thing without human nudges.
3. **Stream corruption is logged, not hidden.** `StateError` and `ToolCallDefect` are warn-and-drop/close, never silent, never panics.

## Debugging

```bash
tail -f ~/headroom-proxy.log
grep '"event":"tool_call_defect"' ~/headroom-proxy.log | tail -5
grep '"event":"sidecar_fallback"' ~/headroom-proxy.log | tail -5
grep '"event":"conversation_concurrency_shed"' ~/headroom-proxy.log | tail -5
grep '"event":"upstream_retry_after_exceeds_cap"' ~/headroom-proxy.log | tail -5

curl -s localhost:8787/healthz                  # liveness: {"ok":true,...}
curl -s localhost:8787/cache-health | head -40  # hit rates, concurrency_sheds_total, upstream verdict
curl -s localhost:8787/stats | head -40         # lifetime_metrics, incl. failed_work by_status
```

!!! note "Python SDK mirror"
    If you came here for `HeadroomError`, `ConfigurationError`, `ProviderError`, `StorageError`, `CompressionError`, or `ValidationError`: those classes are documented for the read-only `upstream-python/` mirror (`upstream-python/headroom/exceptions.py`) and are not raised by anything under `crates/`. Debug the Rust path above instead.
