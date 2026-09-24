# Observability — proxy metrics

The Headroom Rust proxy exposes Prometheus-format metrics on the
`/metrics` endpoint of every running proxy instance. The metric
catalogue below covers Phase D (Bedrock route instrumentation),
Phase G PR-G3 (proxy-wide observability), the CTX-3/4/5/7 cache
families, CCR retrieval, sidecar handling, and upstream health.

All `proxy_*` / `ctx_*` / `offload_*` metric names + label keys are
constants in
`crates/headroom-proxy/src/observability/metric_names.rs`, so any
rename catches one file in code review. The `headroom_*` legacy
families keep inline literals in
`crates/headroom-proxy/src/observability/proxy_counters.rs` (a
Python-parity port — names and HELP text match
`upstream-python/headroom/proxy/prometheus_metrics.py` verbatim so
one dashboard scrapes either side).

!!! note "No `prefix_replay_*` Prometheus family exists"
    Prefix replay is observable in Prometheus through
    `proxy_cache_replay_alternates_evicted_total` and
    `proxy_cache_tail_breakpoint_total{outcome}` below. The
    `prefix_replay_*` names are structured **log events**, documented
    under "Structured events".

## Metric catalogue

### Bedrock route (Phase D PR-D3)

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `bedrock_invoke_count_total` | Counter | `model`, `region`, `auth_mode` | One increment per Bedrock `/invoke` or `/converse` request. |
| `bedrock_invoke_latency_seconds` | Histogram | `model`, `region` | Latency from proxy entry to upstream completion. Buckets target 50ms–60s. |
| `bedrock_eventstream_message_count_total` | Counter | `model`, `region`, `event_type` | One increment per parsed binary EventStream message. |

### Proxy-wide (Phase G PR-G3)

#### Cache + compression

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `proxy_cache_hit_rate_per_session` | Histogram | `provider` | Per-session cache hit rate. **Phase H canary gate.** |
| `proxy_compression_ratio_by_strategy` | Histogram | `strategy`, `content_type` | `compressed_tokens / original_tokens` per shrunk block. |
| `proxy_tokens_saved_total` | Counter | `strategy`, `content_type` | Cumulative input tokens removed from the wire (`original - compressed`, summed per shrunk block). The running "you saved X" total the ratio histogram cannot express. |
| `proxy_compression_rejected_by_token_check_total` | Counter | `strategy` | Compressor ran but failed the shrink check. |
| `proxy_compression_declined_no_shrink_total` | Counter | `strategy` | Compressor ran and the dispatcher declined it because the output was not smaller in bytes. |

`declined_no_shrink` and `rejected_by_token_check` split what used to be one
number. A compressor that rewrites a block without removing anything — the
search compressor re-emitting every match in `BTreeMap` order is the common
case — now stops at the dispatcher instead of travelling to the tokenizer, so
it leaves the rejection counter. Without the second counter that fall would be
unreadable: you could not tell "we stopped wasting runs" from "we stopped
attempting compression that paid".

Read the pair against `proxy_compression_ratio_by_strategy_sum`. Declines
rising while accepted compression holds steady means the size gate is absorbing
waste. Both falling together means it is declining work that pays.

```promql
# Share of runs the size gate absorbed, per strategy.
sum by (strategy) (rate(proxy_compression_declined_no_shrink_total{strategy!="__init__"}[1h]))

# The control: accepted compression volume must not fall with it.
sum by (strategy) (rate(proxy_compression_ratio_by_strategy_sum{strategy!="__init__"}[1h]))
```

#### Recache watchdog (CTX-7)

Turns where usage showed the prompt-cache prefix was re-written inside the
TTL window. Classified by
`crates/headroom-proxy/src/cache_stabilization/usage_observer.rs`,
counted here. The full attribution vocabulary lives under
"/cache-health attribution".

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `proxy_cache_recache_events_total` | Counter | `reason` | One increment per re-cache event. `reason` is the drift axis, replay-skip cause, cache-timing race, or landing class — `system`, `tools`, `early_messages`, `inbound_tail_replaced`, `prefix_head_changed`, `prefix_content_diverged`, `forwarded_beta_rotated`, `forwarded_count_mismatch`, `shorter_than_stored_prefix`, `optimized_shorter_than_prefix`, `concurrent_turn_in_flight`, `aftershock_of_diverged_prefix`, the five `provider_*` landing classes, replay-skip names (`no_previous_turn`, `inflated_without_confirmed_floor`, `system_adjacency_broken`, …) / `no_cause_found`, `multi` (comma-joined dims), `structural_drift` (future dimension), `unknown`. |
| `proxy_cache_recache_wasted_tokens_total` | Counter | _none_ | Billed tokens wasted re-writing prefixes that should have been reads (summed `wasted_tokens` across charged events only — branch builds and unattributed `Expected` events contribute 0). |
| `proxy_cache_first_turn_write_tokens_total` | Counter | `reason` | `cache_creation_input_tokens` billed on the first completed turn under each conversation key, which the recache classifier has no previous turn to score against. `reason` in precedence order: `compaction_restart`, `session_key_drift`, `identical_prompt_fanout`, `fresh_session`, `arrived_with_history` (anything else collapses to `unknown`). Not waste — a cold start has nothing to read — but the largest unattributed bucket before it was counted. |

```promql
# Waste rate by cause, top offenders first.
topk(5, sum by (reason) (rate(proxy_cache_recache_events_total[1h])))

# First-turn write share of all cache creation (needs provider usage totals).
sum(rate(proxy_cache_first_turn_write_tokens_total[1h]))
```

#### Prefix replay

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `proxy_cache_replay_alternates_evicted_total` | Counter | _none_ | Stored branch prefixes dropped because the alternates count/message budget was full. One session key carries several interleaved streams; an evicted stream busts on its next turn. |
| `proxy_cache_tail_breakpoint_total` | Counter | `outcome` | Anthropic requests the tail-breakpoint stage looked at. `applied` = moved the message marker, `skipped` = already placed (the common case) or refused. `applied` against `skipped` is the only way to tell "working" from "never firing". |

!!! note "Wire name vs constant name"
    The tail-breakpoint constant is
    `METRIC_PROXY_CACHE_BREAKPOINT_SPREAD_TOTAL` but the wire name is
    `proxy_cache_tail_breakpoint_total`. Query the wire name.

#### Cache-safety alarm

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `proxy_passthrough_bytes_modified_total` | Counter | `path` | Bytes mutated on a passthrough path. **Must stay 0 outside the compression hot path** — any non-zero rate fires the cache-safety alarm. |

!!! warning "Cache-safety alarm"
    Wired in `crates/headroom-proxy/src/proxy.rs`: when the dispatcher
    returns `Outcome::NoCompression` or `Outcome::Passthrough`, the
    post-dispatcher byte length is compared to the original buffered
    length and any delta increments the counter (by the byte delta)
    under the request's path label. The PR-E4 prompt_cache_key
    injector runs AFTER the alarm check, so its intentional byte
    mutations do not trip the alarm.

#### Upstream rate limits

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `proxy_rate_limit_remaining_requests` | Gauge | `provider` | Last-seen remaining requests in the current window. |
| `proxy_rate_limit_remaining_tokens` | Gauge | `provider` | Last-seen remaining tokens in the current window. |
| `proxy_rate_limit_remaining_input_tokens` | Gauge | `provider` | Anthropic-only input-token bucket. |
| `proxy_rate_limit_remaining_output_tokens` | Gauge | `provider` | Anthropic-only output-token bucket. |
| `proxy_ratelimit_unified_utilization` | Gauge | `window` | Consumed fraction [0,1] of a Claude-subscription window, from `anthropic-ratelimit-unified-<window>-utilization`. `1 - value` is the remaining headroom. `window` is dynamic (`5h`, `7d`, per-model like `7d_sonnet`). |
| `proxy_ratelimit_unified_reset_seconds` | Gauge | `window` | Unix epoch at which the window resets. `overall` carries the top-level reset. |
| `proxy_ratelimit_unified_throttled` | Gauge | `window` | 1 when the window status is anything other than `allowed`, else 0. `overall` carries the top-level status; `overage` carries the overage status. |
| `proxy_ratelimit_unified_fallback_percentage` | Gauge | _none_ | Share [0,1] of traffic upstream steers to fallback capacity. |

The `*-remaining` family covers API-key traffic; subscription/OAuth
traffic returns the `unified-*` family instead, which is why the
`*-remaining` gauges stay empty on a subscription deployment. Overage
strings and the representative claim are log-only (paired with each
update on `event="metric_recorded"`); per-window utilization moves
also emit `event="unified_utilization_sample"` at INFO.

#### Upstream retries and truncated streams

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `proxy_upstream_retries_total` | Counter | `path`, `reason` | Requests re-sent after a transient upstream failure. `path` is `anthropic` or `routed`; `reason` is `status_429`, `status_529`, `status_5xx`, `transport` or `in_band_sse`. One increment per re-send. |
| `proxy_upstream_retries_exhausted_total` | Counter | `path`, `reason` | Turns the retry loop gave up on. Same labels as above, so the two divide: retries say it happened, exhausted says whether the budget covered the outage. Every count here is a whole turn lost. |
| `proxy_stream_incomplete_total` | Counter | `provider` | SSE streams that ended without their terminal event. |

Retries cost latency and re-bill the input tokens, so a rising rate
is a bill, not just noise. Both retry loops honour `--retry-max-attempts`
(default 3) and increment this counter next to the backoff sleep.

`in_band_sse` is the reason worth watching. Anthropic answers rate limits and
overload on a streaming request with HTTP 200 and an SSE body whose first event
is `{"type":"error",...}`. A retry loop reading `r.status()` alone cannot see
it, so those turns looked like success and spent none of the retry budget. The
proxy now peeks the first event and re-sends when it opens with
`overloaded_error`, `rate_limit_error` or `api_error`. Only a *leading* error
qualifies — once content has been forwarded, retrying would duplicate it, and
those streams end without `message_stop` and land in
`proxy_stream_incomplete_total` instead.

`proxy_stream_incomplete_total` counts the turns missing from the cost
and savings books. Anthropic reports the turn's final `output_tokens`
on the `message_delta` before `message_stop`; a stream cut short by a
client disconnect carries a partial count, so those turns are dropped
rather than booked at a figure that flatters them. The partial numbers
stay in the structured log (`event="stream_incomplete"`).

#### Upstream health

A rejected turn is the most expensive outcome there is: the work is
lost, the client retries, and the cached prefix is written again.

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `proxy_upstream_responses_total` | Counter | _none_ | Every upstream response, any status. The denominator the rejection count alone cannot supply. |
| `proxy_upstream_rejections_total` | Counter | `status` | Non-2xx responses, by HTTP status (`429` kept separable by its label). Costlier than any cache miss. |

Rate limiting is excluded from the *alert*, not the count: a 429 is
the provider throttling a healthy proxy. The rolling 50-response
window escalates (`event="upstream_rejection_rate_high"`, ERROR) only
on 4xx-other-than-429 at ≥10% with ≥3 rejections, rate-limited to one
line per 5 minutes. 5xx is counted but never escalated — the provider
failing is not the proxy sending something unusable. The current
window also ships in `/cache-health` under `upstream`.

#### OpenAI Responses telemetry

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `proxy_service_tier_count_total` | Counter | `tier` | Service-tier distribution observed at the proxy. |
| `proxy_response_status_count_total` | Counter | `status` | Terminal status distribution (`completed`, `incomplete`, `failed`, `cancelled`, `in_progress`). |

#### Spinner sidecars

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `proxy_sidecar_total` | Counter | `kind` | Sidecar requests answered on a shrunk request instead of forwarded whole. `describe_action` = Claude Code's spinner line (resends the whole conversation for four words of output); `fallback` = shrunk request failed and went whole after all — should stay near zero. |

#### CTX offload, recall, and search (CTX-3/4/5/6)

Offload removes bytes, proactive expansion puts them back — read
either alone and the answer flatters. The byte counters are also
served as plain values on `/ctx/stats`.

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `ctx_offloaded_bytes_total` | Counter | _none_ | Bytes offloaded from `tool_result` blocks into the CCR store. |
| `ctx_offloaded_blocks_total` | Counter | _none_ | Blocks offloaded (replaced with a digest). |
| `ctx_offload_index_pending_jobs` | Gauge | _none_ | Durable CTX-3 FTS jobs waiting for completion. |
| `ctx_offload_index_pending_bytes` | Gauge | _none_ | Original-content bytes held by the durable CTX-3 index outbox. |
| `ctx_offload_index_oldest_age_seconds` | Gauge | _none_ | Age of the oldest pending CTX-3 index job. |
| `ctx_offload_index_batches_total` | Counter | _none_ | Per-project FTS batch attempts by the background index worker. |
| `ctx_offload_index_batch_duration_seconds` | Histogram | _none_ | Time spent attempting one per-project CTX-3 index batch. |
| `ctx_offload_index_retries_total` | Counter | _none_ | Batches deferred for retry after an index write failure. |
| `ctx_offload_index_backpressure_total` | Counter | _none_ | Requests refused by the bounded index outbox. |
| `ctx_offloaded_blocks_by_tool_total` | Counter | `tool` | Offloaded blocks by producing tool (`Read`, `Grep`, `Glob`, `Write`, `Edit`, `WebSearch`, `WebFetch`, `Bash`, else `other`). Answers whether file/search results convert or only Bash output does. |
| `ctx_recall_injections_total` | Counter | _none_ | Recall/resume blocks injected into the first user message (CTX-4 engine). |
| `offload_gate_seeded_total` | Counter | _none_ | Newborn sessions seeded from the same conversation's prior session (model switch, resume). Seeded sessions convert known blocks on first sight instead of stalling Deferred. |
| `offload_gate_seed_refused_live_total` | Counter | _none_ | Seedings refused because the gate already knew the session. Refusal is the safe outcome; rising refusals beside zero seedings means the birth signal misfires. |
| `ctx_proactive_expansion_bytes_total` | Counter | _none_ | Previously-offloaded bytes appended back into the latest user turn. The counterweight to `ctx_offloaded_bytes_total`. |
| `ctx_proactive_expansions_total` | Counter | _none_ | Requests with at least one offloaded block re-inserted. |
| `ctx_proactive_expansion_cache_write_tokens_total` | Counter | _none_ | Provider-reported cache-creation tokens on expansion requests — the actual write charged, not estimated from bytes. |
| `ctx_injection_clipped_bytes_total` | Counter | `stage` | Bytes dropped by the shared per-request injection budget (`proactive_expansion` / `recall` / `memory`). Non-zero means the stages want more room than `--max-injection-bytes` allows. |
| `ctx_retrieval_hits_total` | Counter | _none_ | `/ctx/get` retrievals that found the original in the CCR store. |
| `ctx_retrieval_misses_total` | Counter | _none_ | `/ctx/get` retrievals for an absent hash (expired, evicted, never offloaded). A rising rate flags information-loss risk. |
| `ctx_search_queries_total` | Counter | _none_ | Search queries served via `/ctx/search`. |
| `proxy_ctx_events_deduped_total` | Counter | _none_ | Session events offered twice that the store refused (`session_id, type, data_hash` already present). A steady rate means the incremental-capture high-water mark is not holding. |

#### CCR retrieval outcomes

`ctx_retrieval_hits_total` counts the store lookup, not whether the
content ever reached the model. These say what became of each
buffered `headroom_retrieve`.

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `proxy_ccr_retrieval_outcomes_total` | Counter | `outcome` | Terminal fate: `continuation` (normal — second upstream call carried it back), `spliced_mixed` (shared the turn with a real client tool call, went in as text), `spliced_failed` (all-failed splice, fell through to continuation), `unresolved` (asked and got nothing — must stay 0). |
| `proxy_ccr_continuation_retries_total` | Counter | _none_ | Continuation POSTs re-sent after transport error / 5xx / 429. Rising retries with flat `unresolved` means the backoff works. |
| `proxy_ccr_cross_project_hits_total` | Counter | _none_ | Blocks the CCR store missed that the per-project content index recovered from the cold tier. Each one saved a continuation round. |
| `proxy_ccr_local_tier_hits_total` | Counter | _none_ | Same expiry shape, recovered from the requesting project's own index (the sweep skips it by design). True information loss reads as `ctx_retrieval_misses_total` minus both hit counters — never from the miss counter alone. |
| `proxy_ccr_splice_dropped_blocks_total` | Counter | `reason` | Blocks the retrieval splice refused to forward: `unresolved_proxy_tool` and `continuation_thinking` are routine; `already_streamed` (client already has it under a new index) has no legitimate cause and is the only reason `/cache-health` counts. |

#### Redaction

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `proxy_redact_restore_misses_total` | Counter | _none_ | `--redact-sensitive` placeholders that reached the client unresolved. Emitted as an unresolved marker, never a raw token — still a defect every time. |

#### Image log redaction (Python-side)

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `proxy_image_generation_call_log_redacted_total` | Counter | _none_ | Base64-encoded image payloads redacted from request logs. Lives on the **Python** proxy's `/metrics` (`upstream-python/headroom/proxy/prometheus_metrics.py`), not the Rust scrape below. |

> **C3 remediation:** Image redaction is purely a Python-proxy
> operation (the request logger walks JSON and replaces over-
> threshold image payloads with placeholders). The counter lives
> Python-side so we have one source of truth instead of two. The
> Rust proxy previously held a dead counter for this metric; that
> has been removed.

#### Legacy request/token/latency families (`headroom_*`)

Python-parity port in `proxy_counters.rs` — same names and HELP text
as the Python exporter, so existing dashboards scrape either side.
`headroom_requests_by_provider` / `headroom_requests_by_model`
deliberately omit the `_total` suffix (Python spelling); both
spellings stand so neither side's dashboards break.

Unlabelled counters:

| Name | Purpose |
|------|---------|
| `headroom_requests_total` | All proxied requests. |
| `headroom_requests_cached_total` | Requests served cached. |
| `headroom_requests_rate_limited_total{source}` | Rate-limited requests. `source="headroom"` is our own limiter (raise the cap); `source="upstream"` is the provider refusing (back off or shard keys). |
| `headroom_requests_failed_total{provider}` | Failed requests, by provider. |
| `headroom_conversation_concurrency_sheds_total` | Turns shed by the per-conversation concurrency cap before forwarding (client retries against a committed prefix — costs nothing, unlike a rate limit). |
| `headroom_tokens_input_total` / `headroom_tokens_output_total` / `headroom_tokens_saved_total` | Input / output / saved token totals. |
| `headroom_cache_bust_total` / `headroom_cache_bust_tokens_lost_total` | Requests that lost cache efficiency to compression, and the tokens it cost. |
| `headroom_inbound_requests_total` / `headroom_inbound_requests_completed_total` | Inbound HTTP accepted / finished (includes aborted). |

Labelled counters:

| Name | Labels | Purpose |
|------|--------|---------|
| `headroom_requests_by_provider` | `provider` | Requests by provider. |
| `headroom_requests_by_model` | `model` | Requests by model (client-supplied; capped at 1024 distinct values, overflow buckets to `other`). |
| `headroom_compressions_by_strategy_total` | `strategy` | Compressions run per strategy. |
| `headroom_tokens_saved_by_strategy_total` | `strategy` | Tokens saved per strategy. |
| `headroom_cache_read_tokens_total` / `headroom_cache_write_tokens_total` | `provider` | Provider cache read/write tokens. |
| `headroom_cache_miss_attribution_total` | `provider`, `reason` | Misses on an expected-cached prefix (`ttl_expiry` / `prefix_change` / `unknown`). The Rust usage observer feeds it: TTL expiries → `ttl_expiry` (provider `anthropic`); Drift recaches → `prefix_change`; Unexplained/Expected → `unknown`. Branch builds emit nothing. |
| `headroom_compression_failed_total` | `reason` | Fail-open compression failures. |
| `headroom_kompress_size_gate_total` | `outcome` | Kompress size-gate decisions (`within` = gate pass, not that ML compression ran). |
| `headroom_compression_quarantine_total` | `event` | Timeout-debt quarantine events. |
| `headroom_waste_signal_tokens_total` | `signal` | Tokens attributed to detected waste signals. |
| `headroom_cache_write_ttl_tokens_total` / `headroom_cache_write_ttl_requests_total` | `provider`, `ttl` | Cache-write tokens/requests by observed TTL bucket (`5m` / `1h`). |
| `headroom_uncached_input_tokens_total` | `provider` | Input tokens not served from provider cache. |
| `headroom_provider_cache_requests_total` / `headroom_provider_cache_hit_requests_total` | `provider` | Requests with cache observations / with cache reads. |
| `headroom_provider_cache_bust_total` / `headroom_provider_cache_bust_write_tokens_total` | `provider` | Anthropic-only bust heuristic (skips each model's first cached request; flags writes > half of read+write) and its write cost. |
| `headroom_stage_timing_ms_sum` / `_count` | `path`, `stage` | Per-stage handler timings (sum/count; max below). |
| `headroom_transform_timing_ms_sum` / `_count` | `transform` | Per-transform timings (sum/count; max below). |

Histograms and gauges:

| Name | Type | Labels | Purpose |
|------|------|--------|---------|
| `headroom_latency_ms` | Histogram | `provider`, `model` | Request latency, ms. Exact min/max in the `_min`/`_max` gauges below. |
| `headroom_overhead_ms` | Histogram | `provider`, `model` | Proxy processing overhead, ms (only observed when positive — zero means "not measured"). |
| `headroom_ttfb_ms` | Histogram | `provider`, `model` | Time to first byte, ms (same positive-only rule). |
| `headroom_ws_session_duration_ms` | Histogram | `cause` | Codex WS session duration, ms. |
| `headroom_latency_ms_min` / `_max`, `headroom_overhead_ms_min` / `_max`, `headroom_ttfb_ms_min` / `_max` | Gauge | _none_ | Exact extrema (a histogram cannot give minima). Global, matching Python. |
| `headroom_transform_timing_ms_max` | Gauge | `transform` | Max per-transform timing. |
| `headroom_stage_timing_ms_max` | Gauge | `path`, `stage` | Max per-stage timing. |
| `headroom_ws_session_duration_ms_max` | Gauge | `cause` | Max WS session duration per cause. |
| `headroom_active_ws_sessions` / `headroom_active_relay_tasks` / `headroom_inbound_requests_active` | Gauge | _none_ | Current active counts. |

## Structured events

Every metric increment has a paired log line; these are the ones to
grep. Field lists are the identifying fields, not the full line.

### Cache events (usage observer)

| Event | Level | Key fields | Meaning |
|-------|-------|-----------|---------|
| `cache_recache_observed` | WARN (`drift`, `unexplained`) / INFO (`branch`, `expected`) | `request_id`, `conversation_key`, `session_key_hash`, `matched_stream_msgs`, `turn_msgs`, `streams_tracked`, `drift_dims`, `replay_skipped`, `attribution_reason`, `origin`, `scope`, `event_kind` (`drift`/`branch`/`unexplained`/`expected`), `wasted_tokens`, `expected_cache_read`, `actual_cache_read`, `cache_creation_input_tokens`, `prefix_head/body/stable(_msgs)`; drift adds `first_diff_index`, `prior/current_message_count`; unexplained adds `landing`, `replayed_prefix`, `replay_chain_id`, `breakpoints_placed`, `system_markers_dropped`, `previous/forwarded_request_bytes`, `previous_cache_read/boundary`, `previous_previous_boundary`, `forward_beta/markers`, `beta/markers_changed`, `commit_race_suspect`, `sibling_completed_recently`; branch adds `uncharged_shortfall_tokens` | The re-cache event itself. Emitted per kind with the field set that kind can prove. |
| `first_turn_write_observed` | INFO | `request_id`, `conversation_key`, `session_key_hash`, `msgs`, `cache_creation/cache_read_input_tokens`, `model`, `attribution_reason`, `contradicts_itself`, `adopted`, `donor_session_key_hash` | First completed turn under a conversation key wrote cache. `contradicts_itself` = a `fresh_session` that read cache, or an `arrived_with_history` that read none — a live conversation rebuilding under a new key, filed as a cold start. |
| `cache_recache_ttl_expiry` | INFO | `request_id`, `conversation_key`, `session_key_hash`, `matched_stream_msgs`, `turn_msgs`, `streams_tracked`, `cache_creation_input_tokens`, `idle_seconds` | Prefix re-written after idle > 5 min. Expected, not a defect — the legitimate cache loss to read recache waste against. |
| `unearned_cache_write_observed` | WARN | `request_id`, `turn_class`, `conversation_key`, `session_key_hash`, `cache_creation/cache_read_input_tokens`, `previous_footprint`, `earned/unearned_tokens` | A healthy/first/recache turn re-covered footprint the conversation already held. Fires above the floor only. |

### Prefix replay events

| Event | Level | Key fields | Meaning |
|-------|-------|-----------|---------|
| `prefix_replay_applied` | INFO | `request_id`, `replayed_prefix`, `chain_id`, `breakpoints_placed`, `system_markers_dropped` | Stored prefix rewritten onto the forwarded messages. |
| `prefix_replay_not_replayed` | INFO | `request_id`, `session_key_hash`, `reason` (ReplaySkip name), `miss_detail` (only for `no_previous_turn`), `proxy_uptime_seconds`, `first_diff_index`, churning-field path | Replay declined; names where the histories diverged. |
| `prefix_replay_skipped` | WARN (unparseable body) / DEBUG (`no_messages_array`) | `request_id`, `reason`/`error` | Body never reached the replay decision. |
| `prefix_replay_invalidated_on_rebuild` | — | — | Stored prefix dropped on tracker rebuild. |
| `prefix_replay_side_errand_not_parked` | — | — | Side-errand turn not parked for replay. |
| `prefix_replay_serialize_failed` | — | — | Stored overlay failed to serialize. |

### Newer pipeline events

| Event | Level | Key fields | Meaning |
|-------|-------|-----------|---------|
| `sidecar_detected` | INFO | `request_id`, `kind` (`describe_action`), `original/forwarded_messages`, `model_from`, `model_to`, `routed` | Spinner-text sidecar answered on a shrunk request. Pairs with `proxy_sidecar_total`. |
| `first_turn_write_observed` | INFO | (see cache events above) | Pairs with `proxy_cache_first_turn_write_tokens_total`. |
| `forwarded_prefix_length_changed_after_replay` | WARN | `request_id`, `messages_before`, `messages_after` | A stage after prefix replay added or removed messages, shifting every later message. Within-turn preamble/content checks were removed as noise (deterministic stages always move bytes); the count check stayed because only a length change breaks the cache. |

### Upstream / stream events

| Event | Level | Key fields | Meaning |
|-------|-------|-----------|---------|
| `stream_incomplete` | WARN | — | SSE stream ended without `message_stop`; turn dropped from the cost books. Pairs with `proxy_stream_incomplete_total`. |
| `upstream_rejection_rate_high` | ERROR | `rejected`, `of_last`, `rate_pct`, `status`, `error_type`, `error_message` | 50-response window refuses ≥10% (≥3) with 4xx≠429. At most one line per 5 min. |
| `unified_utilization_sample` | INFO | `request_id`, `window`, `utilization`, `reset_unix`, `status` | Subscription meter moved (emitted on change, ~1% steps). |
| `passthrough_bytes_modified` | WARN | `path`, `bytes`, `request_id` | The cache-safety alarm firing. Pairs with `proxy_passthrough_bytes_modified_total`. |
| `service_tier_unknown` | WARN | `raw`, `bucket="other"` | Unrecognised `service_tier` bucketed to `other` to bound cardinality. |
| `metric_recorded` | DEBUG | `metric`, labels, `request_id` | Paired with Bedrock / rate-limit / service-tier / status increments for incident correlation under `RUST_LOG=headroom_proxy::observability=debug`. |

## /cache-health

`GET /cache-health` (mounted beside `/metrics` in `proxy.rs`) renders
one cheap in-memory snapshot for statusline polling: the usage
observer's `CacheHealthSnapshot` plus an `upstream` sub-object from
`upstream_health::snapshot()` (lifetime response/rejection totals,
recent 50-response window refusals + rate, last refusal's
status/type/message, verdict `healthy`/`elevated`/`refusing`, and
`ccr_unusable_blocks` — non-zero beside `refusing` is the diagnosis).

Headline fields: `recent_hit_rate` + `samples` (cache-capable turns
only), `recache_events_total`, `recache_wasted_tokens_total`,
`ttl_expiries_total`, `earned/unearned_cache_write_tokens_total` (+
`unearned_write_turns_total`, `productive_write_pct`),
`first_turn_writes_total` / `first_turn_write_tokens_total` /
`first_turn_contradictions_total`, `abandoned_requests_total`,
`concurrency_sheds_total`, hot-zone trio
(`hot_zone_changes_total`, `hot_zone_recaches_total`,
`stabilization_absorbed_total` + tokens + `stabilization_absorb_pct`),
vs-stock comparison (`ours/stock_effective_tokens`,
`stock_turns_compared`, `vs_stock_saving_pct[_recent]`,
`predicted_read_error_pct`), recent-window cost
(`recent_cache_read/write_tokens`, `recent_forwarded_bytes`,
`recent_cost_per_forwarded_kb`), and `last_event` (the most recent
`RecacheEvent`:
`at_unix`/`conversation_key`/`session_key_hash`/`drift_dims`/`attribution_reason`/`landing`/`origin`/`scope`/`replayed_prefix`/`replay_chain_id`/`breakpoints_placed`/`system_markers_dropped`/`previous/forwarded_request_bytes`/`forward_beta`/`forward_markers`/`beta_changed`/`markers_changed`/`commit_race_suspect`/`sibling_completed_recently`/`event_kind`/`wasted_tokens`/`cache_creation_input_tokens`/`expected/actual_cache_read`)
with `last_event_age_seconds`.

### Attribution table

How a re-cache event gets its `attribution_reason`, `origin`,
`scope`, `event_kind`, and whether its tokens land in
`proxy_cache_recache_wasted_tokens_total`. Ranked top-down; the first
evidence wins.

| `attribution_reason` | `origin` | `scope` | `event_kind` | Charged as waste | Evidence |
|---|---|---|---|---|---|
| `inbound_tail_replaced` | `inbound` | `final_message` | `branch` (INFO) | No (shortfall reported as `uncharged_shortfall_tokens`) | Replay declined: only the final inbound message differs — a legitimate branch tail, not a bust. Filed only when the hot zone held still; a tail replacement plus a head/drift move is the move's bust. |
| `system` / `tools` / `early_messages` / comma-joined (`multi` on the metric) | `client` | `hot_zone` | `drift` (WARN) | Yes | PR-E6 drift dims on the inbound hash, taken before the proxy touches anything. |
| `prefix_head_changed` | `client` | `hot_zone` | `drift` (WARN) | Yes | Cacheable head (model/system/tools) differs from the stream's previous completed turn. |
| `prefix_content_diverged`, `shorter_than_stored_prefix` | `client` | `stored_prefix` | `drift` (WARN) | Yes | Replay declined: client history no longer continues the stored prefix (mid-prefix edit, or a second stream on one session key). |
| `forwarded_count_mismatch`, `optimized_shorter_than_prefix`, `optimized_shorter_than_originals` | `client` | `stored_prefix` | `drift` (WARN) | Yes | Replay declined: the pipeline dropped messages below the stored prefix. |
| `forwarded_beta_rotated` | `client` | `cache_key` | `drift` (WARN) | Yes | Forwarded `anthropic-beta` header differs from the stream's previous completed turn. Client-supplied, provider cache-key input, invisible to both drift lanes (measured 2026-09-17: 3 busts after 100+ stable turns). Ranked below drift/head/replay evidence, above proxy causes. |
| proxy drift dims (e.g. `0:blocks 2->1`) | `proxy` | `forwarded_hot_zone` | `drift` (WARN) | Yes | Client hot zone held still, ours did not (tool/context injection, replay, breakpoint move). Checked after client drift: when both moved, the client's edit wins. |
| `concurrent_turn_in_flight` | `client` | `provider_cache_timing` | `drift` (WARN) | Yes | Another turn of the conversation was in flight when this one began; the write it should have read may not exist yet. |
| `aftershock_of_diverged_prefix` | `previous_turn` | `replayed_prefix` | `drift` (WARN) | Yes | Replay applied, but the previous turn diverged — one client edit billing twice. |
| `unexplained_after_replay` (+ `landing`: `provider_missed_newest_write`, `provider_partial_of_previous_write`, `provider_free_read_not_persisted`, `provider_dropped_older_entry`, `provider_between_entries`) | `unknown` | `replayed_prefix` | `unexplained` (WARN) | Yes | Replay applied and the read still came back short. The reason stays the residual; `landing` names where the read stopped against the two previous boundaries (newest write missing, read inside the previous write, free read never persisted, older entry dropped, between entries). Boundary, not a proved provider cause. Check `beta_changed`/`markers_changed` (key rotation neither drift lane sees; `markers_changed` compares breakpoint shape — count + kind + TTL — not raw indices, so pure conversation growth no longer flags) and `commit_race_suspect` (previous turn completed just before this one began) before concluding anything. |
| replay-skip name (`no_previous_turn`, `inflated_without_confirmed_floor`, `system_adjacency_broken`, …) or `no_cause_found` | as ranked | as ranked | `unexplained` (WARN) | Yes, if > 0 | Charged waste with no structural cause: the reason carries what the replay decline knew instead of falling through to `expected`. |
| _(none)_ | _(none)_ | _(none)_ | `expected` (INFO) | No | No direct evidence. Unattributed, not benign-by-proof. |

TTL expiry never reaches this table: idle > 5 min short-circuits to
`TurnClass::TtlExpiry` (`ttl_expiries_total` +
`headroom_cache_miss_attribution_total{reason="ttl_expiry"}`).

## How to query

The proxy renders Prometheus text-format on `GET /metrics`:

```bash
curl -s http://127.0.0.1:8787/metrics
curl -s http://127.0.0.1:8787/cache-health | jq .
```

### Phase H canary gate

The canary script that decides "ship Rust, retire Python" uses
**all four** of these queries against `proxy_cache_hit_rate_per_session`
to confirm parity vs the Python baseline. A single percentile is
not enough — a regression that only shows up at the tail (a small
class of long sessions losing cache hits) would slip through a
median-only check.

```promql
# p50, p95, p99 of cache hit rate over the last 5 minutes, per provider.
histogram_quantile(0.50, sum by (provider, le) (rate(proxy_cache_hit_rate_per_session_bucket{provider!="__init__"}[5m])))
histogram_quantile(0.95, sum by (provider, le) (rate(proxy_cache_hit_rate_per_session_bucket{provider!="__init__"}[5m])))
histogram_quantile(0.99, sum by (provider, le) (rate(proxy_cache_hit_rate_per_session_bucket{provider!="__init__"}[5m])))

# Mean cache hit rate over the last 5 minutes, per provider. The
# `sum / count` form is the cleanest "average without a quantile"
# query and is what the Python baseline reports.
sum by (provider) (rate(proxy_cache_hit_rate_per_session_sum{provider!="__init__"}[5m]))
  /
sum by (provider) (rate(proxy_cache_hit_rate_per_session_count{provider!="__init__"}[5m]))
```

The canary fails if ANY of `p50`, `p95`, `p99`, or `mean` regresses
below the Python baseline for any provider over the canary window.

### Other common queries

```promql
# Cache-safety alarm. Should always be 0 (post-`__init__` row).
sum(rate(proxy_passthrough_bytes_modified_total{path!="__init__"}[5m]))

# Per-strategy compression value at p50 (post-H1 fix: each strategy
# reports its own before/after; pre-fix this was the same aggregate
# ratio repeated per strategy).
histogram_quantile(0.50, sum by (strategy, le) (rate(proxy_compression_ratio_by_strategy_bucket{strategy!="__init__"}[1h])))

# Per-strategy compression value at p95 and p99 (catch outlier
# strategies that fail to shrink at the tail).
histogram_quantile(0.95, sum by (strategy, le) (rate(proxy_compression_ratio_by_strategy_bucket{strategy!="__init__"}[1h])))
histogram_quantile(0.99, sum by (strategy, le) (rate(proxy_compression_ratio_by_strategy_bucket{strategy!="__init__"}[1h])))

# Strategies that ran but failed the token-check (compressor ran
# but its output was not strictly smaller, so the original was
# kept). High rate here means the compressor needs tuning.
sum by (strategy) (rate(proxy_compression_rejected_by_token_check_total{strategy!="__init__"}[1h]))

# Recache waste rate and its top cause.
sum(rate(proxy_cache_recache_wasted_tokens_total[1h]))
topk(3, sum by (reason) (rate(proxy_cache_recache_events_total[1h])))

# Tail-breakpoint health: applied share of requests it looked at.
sum(rate(proxy_cache_tail_breakpoint_total{outcome="applied"}[1h]))
  /
sum(rate(proxy_cache_tail_breakpoint_total[1h]))

# Retry exhaustion share: turns lost after the budget ran out.
sum by (path, reason) (rate(proxy_upstream_retries_exhausted_total[5m]))
  /
sum by (path, reason) (rate(proxy_upstream_retries_total[5m]))

# Upstream rejection rate (non-2xx share, all statuses).
sum(rate(proxy_upstream_rejections_total[5m]))
  /
sum(rate(proxy_upstream_responses_total[5m]))

# Subscription headroom (1 = full window left).
1 - proxy_ratelimit_unified_utilization{window="5h"}

# Upstream rate-limit headroom (smaller = closer to throttle).
proxy_rate_limit_remaining_tokens{provider="anthropic"}

# CCR retrieval failure rate (must stay 0).
sum(rate(proxy_ccr_retrieval_outcomes_total{outcome="unresolved"}[5m]))

# Image-redaction rate (Python-side).
rate(proxy_image_generation_call_log_redacted_total[5m])
```

All `proxy_*` queries above include a `{... != "__init__"}` filter so
the sentinel zero-rows the boot-touch contract emits do not skew the
result. See "Wiring → H3 force-zero" below. The lazy families
(recache, replay alternates, sidecar, CCR, tail breakpoint, upstream
health, retrieval, injection-clipped, by-tool, redact) carry no
sentinel — they appear after their first event, so no filter is
needed.

## Wiring

Every metric registration is `OnceLock`-backed and lazy: the first
call to a `*_counter()` / `*_gauge()` / `*_histogram()` helper
registers the family with the shared registry.

### H3 force-zero

The `prometheus` crate v0.14 skips empty MetricVecs from `gather()`
entirely — neither HELP/TYPE lines nor rows appear until the
family has been incremented at least once with a label tuple.
Operators expect to see the catalogue from boot, so
`handle_metrics` force-touches the hot-path families before the
first scrape:

* `proxy_metrics` counters/gauges (`rejected`, `tokens_saved`,
  `declined_no_shrink`, `passthrough`, rate-limit `*-remaining`,
  `service_tier`, `response_status`, unified utilization/reset/
  throttled, `retries`, `stream_incomplete`) with a sentinel
  `__init__` label tuple (`inc_by(0)` / `set(0)`), plus plain
  `unified_fallback_percentage` (registers with a 0 sample, no
  sentinel needed).
* every `proxy_counters` (`headroom_*`) family via
  `force_register_all` — counters/gauges with `__init__`, histograms
  (`headroom_latency_ms`, `headroom_overhead_ms`, `headroom_ttfb_ms`)
  with a single latched `observe(0.0)` that does not repeat per
  scrape.
* plain CTX-5/6 counters (`ctx_offloaded_*`,
  `ctx_proactive_expansion_*`, `ctx_recall_injections_total`,
  `ctx_search_queries_total`) by calling their getters.

Counters with the `__init__` label increment by 0, so the
alarm-able "must stay 0" semantic of
`proxy_passthrough_bytes_modified_total` is preserved (the family
becomes visible, the rate stays 0). PromQL queries should filter
`{... != "__init__"}` so the sentinel rows are excluded from
aggregations (the catalogue above does this).

!!! note "Lazy families"
    The two PR-G3 histograms (`proxy_cache_hit_rate_per_session`,
    `proxy_compression_ratio_by_strategy`) are NOT force-zeroed: a
    synthetic `observe(0.0)` would contribute a real sample and
    pollute percentile readings. They surface after the first real
    session, by design. The same holds for every family that only
    registers on its first event: recache + first-turn-write,
    replay alternates, sidecar, CCR, tail breakpoint, upstream
    responses/rejections, retrieval hits/misses, injection-clipped,
    by-tool offload, and redact restore-misses. Absence from the
    scrape means "no event yet", not "not instrumented".

### H4 prometheus crate version pin

The H3 contract above relies on the `prometheus` crate's v0.14
`gather()` semantics — empty MetricVec families are omitted from
the scrape. **This is implementation-defined behaviour.** If
`crates/headroom-proxy/Cargo.toml` ever bumps the `prometheus`
dependency, retest the alarm contract:

1. Start a fresh proxy.
2. `curl /metrics` and confirm every force-touched counter / gauge
   family has HELP/TYPE + an `__init__` row.
3. Confirm PR-G3 histograms (`*_cache_hit_rate_per_session`,
   `*_compression_ratio_by_strategy`) and the lazy families
   (`proxy_cache_recache_events_total`,
   `proxy_cache_tail_breakpoint_total`, `proxy_sidecar_total`,
   `proxy_upstream_responses_total`, …) DO NOT appear (no
   `observe()` / event yet).
4. Drive one cache-hit session, scrape again, confirm histograms
   now appear.
5. Confirm `passthrough_bytes_modified_total` stays at 0 across
   passthrough requests.

The crate version is pinned exactly (`= "0.14.0"`, no caret) in
`Cargo.toml` precisely so a silent semver bump cannot break the
contract without a code-review trigger.

### C2 alarm wiring

`proxy_passthrough_bytes_modified_total` fires from `proxy.rs` when
a dispatcher arm that promised byte-equal passthrough
(`Outcome::NoCompression` or `Outcome::Passthrough`) produces a
final body of a different byte length. The check runs BEFORE the
PR-E4 prompt_cache_key injector so the injector's intentional byte
mutations do not trip the alarm.

### H1 per-strategy ratio wiring

`proxy_compression_ratio_by_strategy` samples one observation per
strategy using the strategy's OWN before/after token counts
(plumbed through `Outcome::Compressed.per_strategy_tokens` from
the manifest in `live_zone_anthropic` / `live_zone_openai` /
`live_zone_responses`). Pre-H1 the same aggregate ratio was
emitted per strategy when multiple strategies ran on one body,
making Phase H per-strategy dashboards read garbage.

### H2 aborted-stream gate

The `proxy_cache_hit_rate_per_session` histogram observes ONLY
when the SSE stream completed:

* Anthropic: `state.status == StreamStatus::MessageStop` after the
  channel closes.
* OpenAI Chat: `state.usage.is_some()` (the final usage chunk only
  arrives at stream completion).
* OpenAI Responses: `state.terminal_status().is_some()`.

A client disconnect mid-stream closes the channel without setting
the terminal flag — under H2 we log + skip rather than observe a
garbage half-stream sample.

## Cardinality discipline

Every label vocabulary is bounded by code, not customer input:

- `model` / `region`: read from path params + `Config::bedrock_region`.
- `auth_mode`: 3-variant enum (`payg`, `oauth`, `subscription`).
- `provider`: 3 values (`anthropic`, `openai_chat`, `openai_responses`).
- `strategy`: `&'static str` from the compressor's `BlockAction::Compressed`.
- `content_type`: `&'static str` from `headroom_core::transforms::ContentType`.
- `tier`: validated through
  `crate::observability::metric_names::service_tier::validate(raw: &str)`.
  Returns one of `{auto, default, flex, on_demand, priority, scale}`
  or the sentinel `"other"` for anything else. **The raw inbound
  value is never used as a label.** A malicious client posting
  `{"service_tier":"<random>"}` per request gets bucketed to
  `"other"` and a `tracing::warn!` is emitted so wire-format drift
  surfaces loudly in logs.
- `status`: Responses 5-variant enum; upstream rejections carry the
  numeric HTTP status (bounded vocabulary by construction).
- `reason` (recache): fixed vocabulary in `recache.rs::reason_label`
  — drift dims, replay causes, timing races, the five `provider_*`
  landings, plus `multi` / `structural_drift` / `unknown` buckets so
  future causes cannot widen the set. `headroom_cache_miss_attribution_total`
  collapses further to `ttl_expiry` / `prefix_change` / `unknown`.
- `reason` (first-turn-write): 5 first-turn causes plus `unknown`.
- `tool` (`ctx_offloaded_blocks_by_tool_total`): fixed allowlist of
  8 tools plus `other` — the tool name comes from the request body,
  so anything outside the allowlist collapses.
- `outcome` (CCR retrieval): 4 constants; `outcome` (tail
  breakpoint): `applied` / `skipped`; `kind` (sidecar):
  `describe_action` / `fallback`; `stage` (injection-clipped): the 3
  appender stages; `window` (unified rate limits): dynamic per-model
  window keys from Anthropic headers (`7d_sonnet`-style) — bounded by
  the provider's window set, not by clients.
- `model` (Python-side `requests_by_model` /
  `_cache_requests_by_model`): unlike the Rust path above, the Python
  proxy reads `model` from the request body, so it is client-supplied.
  It is bounded at record time by `MAX_DISTINCT_MODELS`
  (`upstream-python/headroom/telemetry/context.py`): once the cap is
  reached, further distinct models bucket into the `"other"`
  sentinel and a one-time warning is logged, mirroring the `tier`
  discipline above. The Rust `headroom_requests_by_model` and timing
  histograms apply the same 1024-cap + `"other"` rule
  (`proxy_counters.rs`). The in-memory dicts and the exported
  `headroom_requests_by_model` series can never exceed the cap plus
  `"other"`.

Every label vocabulary listed above is bounded by code, so no
client-supplied value can drive label cardinality unbounded.

There is no code path where a malicious client can drive label
cardinality unbounded.

## See also

- `crates/headroom-proxy/src/observability/` — implementation.
- `crates/headroom-proxy/src/cache_stabilization/usage_observer.rs` — classifier + `/cache-health` snapshot.
- `docs/notes/realignment/09-phase-G-rtk-observability.md` — spec.
- `docs/notes/realignment/10-phase-H-python-retirement.md` — H1 acceptance gate.
