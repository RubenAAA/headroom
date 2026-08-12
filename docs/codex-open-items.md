# Codex open items

Status as of 2026-08-12. The selected left items are closed.

| Item | Status | Resolution |
| --- | --- | --- |
| 6 | Closed | Terminal 5xx work is durably recorded outside successful savings. |
| 8 | Closed | Context-injection diagnostics carry the request ID needed to join them to a turn. |
| 12 | Closed | Missing Codex quota is reported once per stream, and the dead prefix-replay latch is gone. |
| 14 | Closed | An over-cap `Retry-After` is returned to the client instead of being shortened into an early retry. |
| 20 | Closed — won't fix | The proxy will not replay content the client withdrew merely to preserve cache reuse. |

## 6 — failed upstream work

Failed turns remain excluded from successful savings, cost, and PERF totals. A
schema-v4 `persistent_savings.failed_work` aggregate records terminal requests,
upstream attempts, request-side forwarded-token estimates, tokens at risk across
attempts, status counts, and optional provider-reported usage. The request-side
estimate is never presented as provider billing.

A controlled 529 with three configured attempts records one failed request,
three upstream attempts, and `forwarded_tokens_at_risk == 3 *
forwarded_tokens`, while successful lifetime totals remain zero. The aggregate
also survives a tracker restart.

## 8 — request-correlated injection diagnostics

The context-injection get, row-miss, persist, search, and build events now carry
the request ID. The compatibility entry point remains available for tests and
non-production callers; both production request paths use the correlated entry
point.

The controlled row-miss regression asserts that the fail-safe still injects
nothing and that its event contains `request_id=req-row-miss`.

## 12 — one-shot quota visibility

A routed Codex stream that ends without quota in either response headers or an
SSE frame emits exactly one `codex_rate_limits_missing` warning with its request
ID. Seeing quota in either location suppresses the warning. Finalization is
latched, so repeated finish/drop paths cannot duplicate it.

The unused `changed` latch and assignments in prefix-replay cache-control
normalization were removed without changing its output.

## 14 — Retry-After compliance

Direct Anthropic and routed Responses retries inspect the uncapped
`Retry-After` value. If honoring it would exceed the configured in-request wait
cap, the proxy makes no early retry and returns the original upstream status,
body, and header. Delays within the cap and ordinary exponential backoff still
retry normally.

The controlled direct and routed cases use `Retry-After: 31`, a 30-second cap,
and three configured attempts. Each makes exactly one upstream request and
returns 429 with header `31`; the routed streaming request is not translated
into a synthetic 200 SSE response.

## 20 — preserve withdrawn client content

No code change. Prefix replay remains all-or-nothing when client-originated
history diverges. Replaying the stored prefix across a divergence could resend
tool-result or other content the client deliberately removed. Semantic
correctness takes precedence over cache reuse, so the safe decline remains the
default.

## Verification

- The staged item 8 snapshot passed the controlled row-miss unit test.
- The staged terminal-response snapshot passed all 8
  `integration_local_model` tests, including the two over-cap cases and
  exhausted-5xx accounting.
- The staged prefix-replay cleanup passed both cache-control normalization
  tests.
