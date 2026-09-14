# Implemented: offload-gap round shipped batch (2026-08-18/21)

- **Status:** shipped (commits `4f223054`, `e796ee3c`, `95df41d0`, `8dc23eb9`, `sse/stream_retry.rs`)
- **Source:** `docs/notes/proxy-experiments-2026-08.md` (blindguard/windowgap corpora)


## Detail

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## Offload-gap round, 2026-08-18 to 08-21 (folded from bench/HANDOFF-offload-gap.md)

Priced on the `blindguard` and `windowgap` corpora described in `bench/README.md`,
`--base forwarded` unless stated. Percentages are against the live proxy.

Proved and shipped:

- **The `--exclude-tools` list was costing about 4pp for nothing.** The
  exclusion exists so the model never acts on a summary of a file it is about to
  edit, which holds for the live-zone compressors but not for offload, where the
  original bytes come back through `headroom_retrieve`. Lifting it took
  `offload-gated-2000` from −10.1% to −13.9% subscription on blindguard and from
  −29.4% to −32.7% API on windowgap. Checked first: 10,415 digest-to-content
  round trips against the client's own re-sent bytes, all byte-identical, none
  missing, no dangling digests across 691 bodies. Shipped in `4f223054`.
  Lifting only the `--exclude-tools` half scored byte-identical to lifting both
  lists on all four runs, so `is_verbatim_excluded` costs nothing and stays.
  Consequence: `stale_margin`'s only remaining reader is the near-tail band
  `distance < margin + window`, so it and `stale_window` now simply add.
- **The CCR tracker cap was too small.** Over 870 requests in 22 sessions the
  digests referenced inside one 300s age window peaked at 91 against a cap of
  100. It evicted 3,953 times in a day, 460 hashes more than once. Cap raised
  100 → 512; lifting the tool exclusions makes roughly 1.8x as many blocks
  eligible.
- **Claude Code's message breakpoint is sometimes short of the tail.** It spends
  three of Anthropic's four markers, two on `system` and exactly one on the
  messages, and that one already sits on the final content block on 97% of
  requests. Moving it forward on the rest is worth −0.9% API and −0.9%
  subscription on blindguard, and exactly zero on windowgap where the client had
  already placed it at the tail on 384 of 389 requests. Shipped in `e796ee3c`
  behind `--cache-tail-breakpoint`, default on, and a no-op when the marker is
  already right.
- **Roughly one buffered `headroom_retrieve` in five never reached the model.**
  Three holes, all quiet, all fixed in `95df41d0`: a turn mixing the retrieval
  with a real client tool call could not run a continuation, so the retrieval was
  dropped and the client got a `tool_use` for a tool it never declared, and the
  content is now spliced in as text instead; a refused continuation was a single
  attempt, and now retries twice with backoff on transport errors, 5xx and 429,
  leaving 4xx alone; and a hash the model mistyped fell out of `parse_tool_call`
  as "not a CCR call", and is now probed raw, recognised, and answered with an
  error.
- **Overload outages run far longer than the retry budget.** Anthropic reports
  overload inside a 200 body when the client asked for a stream. Over five days
  of logs the loop gave up on 77 turns, clustered into 15 bursts running 27 to
  245 seconds, worst case 20 lost turns over four minutes.

  | attempts | waiting | turns cleared |
  | --- | --- | --- |
  | 3 (before) | ~3s | 16 / 77 (21%) |
  | 5 | ~15s | 30 / 77 (39%) |
  | **6 (now)** | **~31s** | **53 / 77 (69%)** |
  | 7 | ~61s | 57 / 77 (74%) |
  | 9 | ~121s | 68 / 77 (88%) |

  Shipped in `8dc23eb9` as `--retry-overload-max-attempts`, default 6, separate
  from `--retry-max-attempts` so nothing else waits longer. The branch can afford
  the wait because the error is the first SSE event, so nothing has been
  forwarded and a re-send cannot duplicate output.
- **Mid-stream transport drops now retry, by holding the opening bytes back.**
  Of 111 streams that ended without `message_stop` across the live log and its
  four rotations, 32 died to `error decoding response body`. Of the 18 that
  correlate to a `stream_incomplete` event, all 18 already had a content block
  open, with 1 to 20 output tokens parsed, median 3. So any design testing "has a
  delta gone out yet" would have declined to retry all 18, and a blind re-send
  would have spliced two generations together. While the held buffer is under
  `--retry-stream-hold-bytes` (default 2048) the response is uncommitted and a
  drop discards it for a fresh request; past that the response is committed and a
  drop propagates as before. This extends the safety condition at `proxy.rs:4124`
  rather than working around it. The wrapper sits below both the CCR rewriter and
  the telemetry tee, so a discarded attempt is invisible to billing. The cost is
  time to first paint: 2 KiB arrives in one burst, which covers the preamble plus
  about a dozen deltas. The drop arrives as reqwest's `Decode`, not `Body`, so
  `is_retryable_transport_error` does not match it, which is why
  `is_retryable_drop` exists. `sse/stream_retry.rs`, wired at `proxy.rs:4400`,
  pinned by `tests/integration_stream_drop_retry.rs`.
