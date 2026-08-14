# Closures: the proxy experiments log

What each item was measured against, and what changed to close it.

Source: `docs/proxy-experiments-2026-08.md` — 2,601 lines, 28 numbered
findings recorded between 2026-07-07 and 2026-08-09. All 26 items tracked here
are closed, all on 2026-08-12: items 16, 1, 2, 10, 15, 9 and 3 first, then 6,
8, 12, 14 and 20.

One warning about that document: its code references have drifted. Item 1 names
`proxy.rs:3144-3147`, which is now unrelated code. Check any line number there
against the current source before trusting it.

## How to work

**Investigate before you change anything.** Every claim below was made from a
log window in early August. The proxy has been rebuilt many times since. Your
first job on each item is to decide whether the defect still exists in today's
code against today's traffic. Several of these will already be fixed. Saying so,
with the query that shows it, is a complete and valuable answer.

**No fix without two numbers.** Before: the measurement that proves the defect
is real now. After: the measurement that proves your change removed it. If you
cannot construct the second one, stop and say why — do not ship a change whose
effect nobody can see. Item 19 in the doc is a fix that was built, measured,
and reverted; item 28 is one that shipped and changed nothing measurable. Both
are better outcomes than an unmeasured fix.

**One item at a time, in the order below.** Items 1, 2, 10 and 15 are four faces
of one problem and will collapse into each other. Fixing 1 is expected to close
2 as well.

**Do not quote item 3's numbers.** They are marked superseded: a share of that
"waste" was one stream being charged for another's prefix under a merged key,
which item 11 fixed. Anything resting on them needs re-deriving first.

## Measurement traps

These have each produced a wrong finding at least once. Violating one wastes a
day and, worse, produces a confident answer.

- **`~/headroom-proxy.log` is never rotated by the restart path.** A plain
  `grep -c` returns a weeks-long total, not the current run. Scope every query
  by date *and* by the running process's start time. Lines are JSON — parse them
  with `json.loads`, not a regex.
- **`/stats` and `/metrics` are process-scoped** and reset on restart, so they
  need no date scoping. The on-disk savings ledger does not reset.
- **Drop each conversation's first turn** when comparing cache cost between
  runs. It writes its whole prefix, scales with how many conversations start,
  and inverts the comparison when left in. Measured 2026-08-11.
- **`prefix_replay_applied` does not mean the replay worked.** It means the
  forwarded bytes changed, which includes turns that declined. The only sound
  test is the absence of `prefix_replay_not_replayed` for the same request id.
  This is item 27, and it invalidated item 21's headline.
- **`cache_recache_observed` carries `event_kind`.** Most events are `expected`
  — a context reset where the re-creation was going to happen anyway. Counting
  them as drift roughly doubles the reported waste. Filter on
  `event_kind == "drift"`.
- **A rejected request does not enter successful savings.** Terminal 5xx turns
  now enter the separate `failed_work` bucket; 4xx responses still do not enter
  it. Neither can inflate the successful savings ledger.

## The items

### First, because it is not an accounting bug

**16 — closed 2026-08-12: the proxy edited the content of tool results.** The
responsible transform was `SearchCompressor`, not `LogTemplate`. Its permissive
grep parser accepted an ISO timestamp as `file:line_number:content`, parsed the
zero-padded minute into a `u64`, then rendered the selected match from those
parts. `:02:` therefore became `:2:`. The fix keeps the raw source line on
`SearchMatch` and emits it verbatim; grouped output removes only the parsed file
prefix and retains the raw remainder. Parsed fields are still used for scoring,
selection and grouping, so nothing downstream depended on losing the padding.

Before/after is pinned in
`selected_lines_preserve_zero_padded_colon_fields_verbatim`: the old renderer
mutates 2 of 3 selected timestamp lines; the current renderer mutates 0 of 3.
`grouped_output_preserves_zero_padded_line_field` covers the alternate output
layout. `integration_digit_integrity` forces the real Anthropic request path to
run `search_compressor` and requires three policy-selected lines to survive
exactly, including padded times, versions, IDs, exit codes and ports; it cannot
pass by skipping compression or omitting the affected lines.

The release artifact containing the fix (SHA-256
`0e7462052ed939a8f117352b60c3e4c9586820a13482d18710457e38c75c0f03`) was
installed at 2026-08-12 12:52:48Z. Through 14:07:02Z, JSON-parsed log records
show 387 forwarded requests and 335 `search_compressor` applications; all 335
were forwarded and reached `sse stream closed`. This is deployment/exercise
evidence, not a live mutation count: the proxy deliberately does not log
outbound tool-result content, so production logs cannot compare those bytes.

### The savings accounting, in this order

**1 — closed 2026-08-12: `tok_after` no longer goes negative.** Scoped from the
running process's start at 12:52:48Z through 14:07:02Z, 383 JSON-parsed PERF
records contain zero negative or zero `tok_after`, zero `tok_saved >
tok_before`, and zero failures of `tok_after == tok_before - tok_saved`. The
minimum `tok_after` is 2. The maximum saving is 78.96% of its baseline; zero
turns reach 95%, so neither an equality placeholder nor a near-baseline clamp
is hiding the old tail. Aggregate arithmetic is also exact:
`1,909,062 - 934,617 = 974,445`.

The attribution is direct, not inferred. Of those records, 335 join by
`request_id` to a structured `compression applied` event; all 335 before,
after and freed triplets agree exactly. Therefore the former
`tok_saved - tokens_freed` excess is zero on every joined turn, rather than a
conversation-sized value repeating across turns. The current code constructs
`OutcomeContext` from the compression dispatcher's own `tokens_before` and
`tokens_after` and books `tokens_saved: compress_tokens_saved`; CTX offload has
separate accounting and is no longer folded into this subtraction. The live
22:40 regression shape remains pinned by
`sizes_books_only_the_compression_turn_so_tok_after_stays_non_negative`. No
code change was needed for this item.

**2 — closed 2026-08-12: the savings operands now share a scope and turn.** In
the current process window, the 335 positive-saving PERF records total
`tok_before=1,314,871`, `tok_after=380,254`, `tok_saved=934,617`. The durable
ledger has exactly 335 rows for the same PID/window and the same three totals;
zero rows fail `before - after == saved`. Including 48 zero-saving PERF turns
changes the comparison to `1,909,062 - 934,617 = 974,445`, still exact. The
former ~100x discrepancy does not reproduce.

The headline definition is now explicit: `headroom savings` reports transform
efficiency on successful compression events. Its denominator is the
pre-compression input selected by those transforms; it is not the whole prompt
or all provider input, and zero-saving turns are absent from this append-only
ledger. The CLI now says `selected tokens`, and the README and measurement guide
state the scope. `/stats.savings_verdict` remains the net proxy calculation
(compression less cache busts); `/stats.wire_verdict` is the whole-request view
paired with provider-reported usage. No accounting-code change was needed.

**10 — closed 2026-08-12: saved-token dollars now use cache placement.** The
current process confirms the defect but not the original 10x magnitude. Joining
335 `savings_placement` and `savings_pricing_counterfactual` events prices
934,617 saved tokens at $14.019255 when every token is called fresh input,
versus $10.776339 using each turn's cache placement: a 1.301x overstatement.
Dropping each conversation's first turn leaves 333 events, 925,603 tokens and
$13.884045 versus $10.641129, or 1.305x. Of those saved tokens, 694,401 classify
past the cache boundary and 240,216 inside the cache-read prefix. The split is
an upper bound on the valuable share: a selected span that fits in the fresh
tail is wholly called fresh, even if an earlier block contributed.

Both durable dollar paths now use that request-scoped placement. The common
`RequestOutcome` selects `fresh_input` or `cache_read` from its forwarded
selected span and provider usage, resolves the matching rate from the pricing
table, and supplies the result to `SavingsTracker` and the append-only savings
ledger. New ledger events carry `cost_basis`, and the structured pricing event
carries both `priced_cost_basis` and `priced_cost_usd`. Token counts and their
denominators are unchanged. Tests pin both placement branches, the tracker
override, and exact ledger serialization. `cargo check -p headroom-proxy`
passes.

The release build was installed and restarted at 14:26:29Z. Through 14:30:44Z,
all 30 saving turns joined one-to-one across `savings_placement`,
`savings_pricing_counterfactual`, and PID 57588's durable ledger rows. Eighteen
were priced as `fresh_input` and 12 as `cache_read`; zero basis/rate joins
disagreed, zero ledger rows failed `before - after == saved`, and the ledger's
banker's-rounded aggregate exactly matched the structured events at $1.008354.
Calling all 109,503 saved tokens fresh would have reported $1.642545, a 1.629x
overstatement. After dropping each of three conversations' first observed turn,
the comparison is $1.444740 versus $0.810551 across 27 events, or 1.782x. The
CLI and documentation now label dollars as estimates and warn that legacy rows
without `cost_basis` retain their former fresh-input assumption; those rows
cannot be honestly repriced because they never recorded placement.

**15 — closed 2026-08-12: routed savings are turn-local and measured.** The old
sample had 156 routed turns, only 14 distinct repeated `tok_saved` values, and
178,006 booked saved tokens despite zero joined `compression applied` events.
The routed handler used one accumulator for CTX offload and live-zone
compression, then booked that aggregate. It now replaces the CTX count with the
current dispatcher's measured count before booking; tool-schema compaction may
then add its own directly measured saving. CTX remains visible as the separate
`ctx_transform_tokens_saved` field on `routed_compression_accounting`.

The current-day live sample is small but fully joinable. The sole routed turn
at 07:56:47Z carried request ID `b8baddf3-577e-4a77-bbf1-09df6b021087` from
`model_route_translate` through accounting and PERF. Live-zone compression and
CTX each reported zero. Tool-schema compaction was the only shrinking transform,
and PERF booked exactly its six-token result: `770 - 764 = 6`, with
`tool_schema_compaction` named in `transforms`. Thus zero of one current routed
turn has an unexplained saving, versus all 178,006 tokens being unexplained in
the original window. `routed_booking_does_not_reemit_ctx_savings_without_compression`
pins the old 4,522-token repeat shape and proves it now books zero;
`routed_compression_actually_shrinks_a_compressible_body` proves the positive
branch runs the real dispatcher and makes the body smaller. Both tests pass.

PERF intentionally retains the upstream model (`gpt-5.6-sol` in this turn),
because pricing must resolve the model that billed the tokens. The route event
now carries both the client-facing alias (`claude-codex-5.6-sol`) and the same
request ID, so the two names join directly and searching only PERF for “codex”
is no longer the required discovery path.

### Blind spots

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

**3 — closed 2026-08-12: the post-item-11 result reverses the old conclusion.**
The superseded window claimed 505,052 drift-waste tokens against 264,247 saved.
On the completed 12:52:48Z–14:26:29Z process window, 412 booked turns instead
saved 1,077,450 tokens and produced two genuine-drift re-cache events totalling
13,926 tokens. Both are unique request IDs on different conversation keys, 26
minutes apart; the duplicate/concurrent-pair signature from item 3a does not
recur. Four `event_kind=expected` resets total 26,322 tokens and are correctly
excluded from waste.

The next process, through the same 14:35:45Z completed-turn cutoff used for
item 9, adds 56 booked turns and 141,908 saved tokens with zero drift-kind
re-cache events. Its three expected resets total 281,386 tokens; including
those would reproduce exactly the measurement trap this brief warns against.
Across both classifiable windows the like-for-like result is therefore
1,219,358 tokens saved, 13,926 lost to drift, and 1,205,432 net saved. Drift is
1.142% of the saving, or savings outweigh measured drift waste by 87.56x. The
old “spent more than it saved” conclusion does not survive the stream-matching
fix and corrected per-turn savings accounting. No code change was needed.

**6 — closed 2026-08-12: failed turns have separate durable books.** The early
return in `emit_request_outcome` remains: a terminal 5xx cannot improve the
successful savings rate, cost totals or PERF population. `record_failed` now
writes a schema-v4 top-level `failed_work` aggregate instead. It records failed
requests, upstream attempts, one-body forwarded tokens, forwarded tokens at
risk across all attempts, status counts, and optional provider-reported input
and output usage. Provider usage is deliberately separate from the request-side
estimate rather than fabricated when a rejection has no usage block. The
aggregate is persisted and exposed under
`/stats.persistent_savings.failed_work`; lifetime, session and project success
totals are untouched.

The controlled before/after uses a 529 upstream and three configured attempts.
Before the wiring, all three upstream requests occurred but the successful
lifetime remained zero and there was no failed-work record; the regression test
failed on `failed_work.requests == 0`. It also exposed a structural gap: the
small non-SSE rejection branch logged `upstream_rejected` but never constructed
a `RequestOutcome`. After the fix, the same run records one failed request,
three upstream attempts, one forwarded-body estimate, and
`forwarded_tokens_at_risk == 3 * forwarded_tokens`; provider usage observed is
zero and all successful request/token totals remain zero. A tracker persistence
test separately pins two failures across statuses 529 and 503, five total
attempts, 51,000 forwarded tokens, 143,000 at risk, and optional actual usage
on only one request.

### Lower stakes

**8 — closed 2026-08-12: injection failures are joinable; the other claims are
not current proxy faults.** Across the 469 completed/currently-classifiable
turns used above there are zero live `ctx_inject_row_miss`, get, persist or
search failures. The source-level gap was real: those events had a conversation
hash but no request ID. A controlled missing-row lookup first reproduced an
unjoinable `ctx_inject_row_miss`; after threading the request ID through the
injection decision/build path, the same event contains
`request_id=req-row-miss`. Get, persist and search failures and successful
builds carry the same key. Production injection budgets are now
request-correlated too, so clipped and overrun events join the turn instead of
becoming a second blind spot.

The remaining notes close without code changes. All 21 non-JSON lines in the
current log are three seven-line entries from `restart-headroom.sh`, not
malformed proxy records. The three non-PAYG skip events each occur 469 times, as
configured for subscription auth. There are 60 current volatile-content
warnings but zero under `tools[]`; the varying tool-index sample does not
reproduce, and current warnings carry both request and conversation keys.

**12 — closed 2026-08-12: the remaining observability gaps are removed.** The
headline was already stale: `ccr/response_handler.rs` warns on an unmatchable
tool-call ID, and its proxy caller logs parse failures, missing hashes, mixed
tools, round limits, continuation errors and residual unresolved calls with a
request ID. Current traffic contains 18 CCR-related records across six message
classes, including three mixed-tool decisions each paired with an explicit
client-resolution classification and stream-splice record. The original
context-tracker, half-persist, semantic-cache eviction/TTL, memory re-index,
purge-error and in-memory CCR eviction gaps likewise have tracing or propagate
their errors; the source audit confirms seven of the nine entries were already
addressed.

Quota observation now emits exactly one joinable
`codex_rate_limits_missing` warning when a routed Codex stream ends without
quota in either response headers or any SSE frame. The controlled missing case
emits one event even when finalization is invoked twice, and carries the request
ID; the positive control with a stream quota object emits zero. This is latched
at stream end rather than warned once per ordinary frame. The discarded
`prefix_replay` `changed` local and its dead assignments were also removed; the
caller-visible decision remains unchanged.

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

**20 — closed 2026-08-12 (won't fix): client withdrawals beat cache reuse.**
The completed pre-restart window has two
`prefix_content_diverged` turns and both join the only two drift-waste events:
message index 2, `content[0].content[0].text`, with `tool_result` shape on both
sides. They cost 5,067 and 8,859 tokens respectively. The post-restart completed
window has zero divergences. Thus 2 of 2 current costly divergences are within
the first five messages, consistent with the original 122 of 164, while the old
thinking-block transition does not recur.

These are changes in client-originated tool-result text, not a boundary the
proxy can safely replay across. Re-sending the stored text would preserve cache
bytes by showing the model content the client no longer sent. The all-or-nothing
guard and regression test therefore remain the safe default. The owner chose
semantic correctness explicitly: never replay withdrawn client content. There
is no code patch for this closure.

## How to measure a change

Build, then put it live: `cargo build --release -p headroom-proxy`, then
`~/restart-headroom.sh` run detached. `/metrics` gives process-scoped counters
that reset with the restart, so the comparison is clean by construction.

Read the result with `headroom savings`, `/stats` (the `savings_verdict` field
subtracts the proxy's own cache busts from its savings), and `/metrics` for
`headroom_cache_read_tokens_total` and `headroom_cache_write_tokens_total`.

**Wait for enough traffic before believing a zero.** For an event class running
at 4% of requests, ~75 requests are needed before an observed zero drops below
5% probability by chance, and ~115 before it drops below 1%. Fourteen requests
proves nothing. Count requests, state the count, and do the arithmetic in your
report.

## State as of 2026-08-12

Shipped today, both with regression tests — do not regress them:

- The proxy no longer places `cache_control` on a `thinking` block. It was
  rejecting whole turns with `messages.N.content.0.thinking.cache_control:
  Extra inputs are not permitted`.
- Every eligible bare string is wrapped to one-text-block form, not only the one
  selected to carry the marker, so a message's shape cannot change when the
  marker moves on.

Measured over 367 requests since that build went live: zero events of the
`[text] -> [string]` reminder-drift class that item 29 targeted, against ~14
expected at the old rate; one `upstream_rejected`, and it is a 429; replay
declines at 3.0%; create/read 0.017 against 0.055 and 0.034 for the two runs
before.

## Ground rules

Never run `git commit` or `git push`. Report what you changed and let the owner
commit.

`observability::ccr_splice::tests::the_summary_ignores_the_routine_reason` fails
about half the time under parallel scheduling — it races another test over the
global Prometheus registry. It is not yours, it predates this work, and it is
worth fixing separately.
