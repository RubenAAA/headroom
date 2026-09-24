# Headroom Metrics — Dashboard Guide

What to scrape and which panels answer which question, for the **Rust proxy**.
For the full metric catalogue (names, types, label vocabularies, wiring), see
[Observability](observability.md) — this page is dashboards and alerts, not the
reference. It does not duplicate the catalogue.

**Scrape one endpoint.** `GET /metrics` on the proxy (default `:8787`) is always
on and renders Prometheus text format. Everything below comes from it except
where a JSON endpoint is named.

!!! note "Filter the boot sentinel out of every query"
    Counter and gauge families are force-touched at boot under an `__init__`
    label so HELP/TYPE lines appear before any traffic. Histograms are not —
    they surface only after the first real sample, by design. Add
    `{...!="__init__"}` to counter/gauge aggregations (all queries below do).

---

## The savings panel — start here

The headline number is compression tokens removed from the wire. The *truth*
number is the ledger.

| Signal | Where | What it shows |
|---|---|---|
| `headroom_tokens_saved_total` | `/metrics` | Cumulative input tokens removed by the live-zone dispatcher. Resets on restart. |
| `headroom_tokens_saved_by_strategy_total{strategy}` | `/metrics` | The same saving, split by compressor strategy. |
| `proxy_tokens_saved_total{strategy,content_type}` | `/metrics` | Per-block saving (`original − compressed`, summed). Same event as above, strategy + content-type cut. |
| `savings_verdict` | `GET /stats` | **Net truth:** `net_tokens_saved = tokens_saved_by_compression − tokens_lost_to_cache_busts`, plus `verdict` (`saving` / `costing more than it saves` / `break-even` / `no data yet`) and `unbooked_turns`. Survives restarts (lives in the on-disk ledger). |
| `wire_verdict` | `GET /stats` | Bytes the proxy put on the wire next to the usage Anthropic reported: `bytes_saved_percent`, `provider_cache_hit_percent`, `bytes_per_billed_token`. Scope is completed Anthropic streaming turns with a usage block — it trails `lifetime_metrics.requests.total` by construction. |
| `lifetime_metrics` | `GET /stats` | Durable lifetime counters (restart-proof). The replacement for any "lifetime tile" — there are no durable savings families on `/metrics`. |

```promql
# Hero tile: tokens saved per second (in-memory; resets on restart)
rate(headroom_tokens_saved_total[5m])

# Context reduction %
100 * rate(headroom_tokens_saved_total[5m])
    / clamp_min(rate(headroom_tokens_input_total[5m]) + rate(headroom_tokens_saved_total[5m]), 1)

# Saving by strategy
sum by (strategy) (rate(headroom_tokens_saved_by_strategy_total{strategy!="__init__"}[5m]))
```

!!! warning "Rate this against the ledger, not against itself"
    `/metrics` counters reset on every restart; the ledger does not (it lives
    under `HEADROOM_WORKSPACE_DIR`, default `~/.headroom`). A dashboard that
    needs "lifetime saved" reads `savings_verdict` / `lifetime_metrics` from
    `/stats`. And `net_tokens_saved` is the number that matters: a proxy can
    report a healthy compression ratio while busting more cache than it saves.
    If `verdict` reads `costing more than it saves`, the compression panels are
    decoration — go to the recache panel.

---

## Latency panel

Rust timings are real histograms with buckets — `histogram_quantile()` works.
Ms families use 10ms–60s buckets; `bedrock_invoke_latency_seconds` uses
50ms–60s buckets.

| Metric | What it shows |
|---|---|
| `headroom_latency_ms{provider,model}` (+ `_bucket/_sum/_count`, `_min/_max` gauges) | Total request duration. |
| `headroom_overhead_ms{provider,model}` | Latency Headroom itself adds. Only sampled when > 0, so its `_count` is smaller than the latency count — divide each `_sum` by **its own** `_count`. |
| `headroom_ttfb_ms{provider,model}` | Time to first byte from upstream. Streaming requests only; same > 0 sampling rule. |
| `headroom_stage_timing_ms_{sum,count,max}{path,stage}` | Where time went inside the handler. Sum/count/max triple, not a histogram. |
| `headroom_transform_timing_ms_{sum,count,max}{transform}` | Time per compression transform. Same triple shape. |
| `bedrock_invoke_latency_seconds{model,region}` | Proxy-entry → upstream-completion for Bedrock routes. |

```promql
# End-to-end p50 / p95
histogram_quantile(0.50, sum by (le) (rate(headroom_latency_ms_bucket[5m])))
histogram_quantile(0.95, sum by (le) (rate(headroom_latency_ms_bucket[5m])))

# Headroom's own overhead, p95 and mean
histogram_quantile(0.95, sum by (le) (rate(headroom_overhead_ms_bucket[5m])))
rate(headroom_overhead_ms_sum[5m]) / rate(headroom_overhead_ms_count[5m])

# Slowest stages (mean)
topk(5, sum by (path, stage) (rate(headroom_stage_timing_ms_sum{path!="__init__"}[5m]))
      / sum by (path, stage) (rate(headroom_stage_timing_ms_count{path!="__init__"}[5m])))
```

---

## Cache panel

Two layers: the provider's prompt cache (counters) and the per-session hit-rate
distribution (histogram). The JSON endpoint adds the fleet view.

| Metric | What it shows |
|---|---|
| `headroom_provider_cache_hit_requests_total{provider}` | Requests that read from the provider cache. |
| `headroom_provider_cache_requests_total{provider}` | Requests with any cache activity. **The denominator for hit rate.** |
| `headroom_cache_read_tokens_total{provider}` | Tokens served from cache (the discounted ones). |
| `headroom_cache_write_tokens_total{provider}` | Tokens written into cache (these carry a premium). |
| `headroom_cache_write_ttl_tokens_total{provider,ttl}` | Writes split by observed TTL — `5m` vs `1h`. |
| `headroom_uncached_input_tokens_total{provider}` | Input tokens that missed cache entirely. |
| `headroom_cache_bust_total` + `headroom_cache_bust_tokens_lost_total` | Requests where compression broke a cached prefix, and the tokens it cost. **Should stay near zero.** |
| `headroom_provider_cache_bust_total{provider}` | Same bust rule per provider (Anthropic only: past the model's first cached request, writes > 50% of read+write). |
| `headroom_cache_miss_attribution_total{provider,reason}` | Why an expected-cached prefix missed — `ttl_expiry`, `prefix_change`, `unknown`. |
| `proxy_cache_hit_rate_per_session{provider}` | Per-session hit-rate histogram (buckets tighten near 0 and 1). The Phase H canary gate — see [Observability](observability.md) for its p50/p95/p99/mean queries. |
| `proxy_cache_tail_breakpoint_total{outcome}` | `applied` vs `skipped`. A few percent applied is healthy; zero over a long run means the stage is not being reached; a sharp rise means the client moved its marker. |

```promql
# Cache hit rate by provider
sum by (provider) (rate(headroom_provider_cache_hit_requests_total{provider!="__init__"}[5m]))
  / sum by (provider) (rate(headroom_provider_cache_requests_total{provider!="__init__"}[5m]))

# Compression breaking cache — alert if this rises
rate(headroom_cache_bust_total[5m])

# Miss reasons
sum by (reason) (rate(headroom_cache_miss_attribution_total{provider!="__init__"}[5m]))
```

!!! note "Don't use `headroom_requests_cached_total` as a hit rate"
    It is one boolean mixing the provider's prompt cache with Headroom's own
    response cache, so it measures neither. Use the provider-cache pair above,
    or `recent_hit_rate` from `/cache-health`.

**Fleet tile from `GET /cache-health`** (rolling window over the last 50
compared turns — `RECENT_SAMPLE_CAPACITY`): `recent_hit_rate` + `samples`,
`recent_cache_read_tokens` / `recent_cache_write_tokens`,
`recent_cost_per_forwarded_kb`, and the `upstream` object (rejection summary,
see alerts). Prefer `vs_stock_saving_pct_recent` + `vs_stock_turns_recent` over
the lifetime `vs_stock_saving_pct` when asking "how is the proxy doing" — the
lifetime figure decays toward the recent one in any long session and reads as a
slide even when nothing got worse.

---

## Recache attribution — where cache money went

A recache event means a prefix was re-written inside the TTL window instead of
read back. First-turn writes are not waste (a cold start has nothing to read)
but they are the largest category, so they get their own counter.

| Metric | What it shows |
|---|---|
| `proxy_cache_recache_events_total{reason}` | Recache events by drift axis (`system`, `tools`, `early_messages`, `inbound_tail_replaced`, timing races like `concurrent_turn_in_flight`, `multi`, `structural_drift`, `unknown`). |
| `proxy_cache_recache_wasted_tokens_total` | Billed tokens wasted re-writing prefixes that should have been reads. Unlabelled — pair it with the events counter for the per-reason split. |
| `proxy_cache_first_turn_write_tokens_total{reason}` | First completed turn per conversation key that wrote cache, by `compaction_restart`, `session_key_drift`, `identical_prompt_fanout`, `fresh_session`, `arrived_with_history`. |
| `proxy_cache_replay_alternates_evicted_total` | Stored branch prefixes dropped because the per-session cap was full — an evicted stream busts instead. |
| `GET /cache-health` | `recache_events_total`, `recache_wasted_tokens_total`, `ttl_expiries_total`, `earned_cache_write_tokens_total` vs `unearned_cache_write_tokens_total` (+ `productive_write_pct`), `hot_zone_changes_total` / `hot_zone_recaches_total` / `stabilization_absorbed_total` (+ `stabilization_absorb_pct`), `first_turn_writes_total`, `first_turn_contradictions_total`, `last_event` + `last_event_age_seconds`. |

```promql
# Recache rate by cause
sum by (reason) (rate(proxy_cache_recache_events_total[5m]))

# Wasted-token burn rate
rate(proxy_cache_recache_wasted_tokens_total[5m])

# First-turn writes by origin (sizing, not waste)
sum by (reason) (rate(proxy_cache_first_turn_write_tokens_total[5m]))
```

!!! note "Read slack before paging anyone"
    The classifier calls a turn healthy when `cache_read` lands within
    `RECACHE_SLACK_TOKENS` (64) of expectation — breakpoint rounding, not a
    recache. Log noise below `UNEARNED_WRITE_FLOOR_TOKENS` (1024) is the same
    class of rounding. A `fresh_session` first turn that *read* cache, or an
    `arrived_with_history` turn that read none, is filed under
    `first_turn_contradictions_total`: a live conversation rebuilding under a
    new key, not a cold start.

---

## Sidecar panel

Claude Code's spinner (`describe_action`) resends the whole conversation for
four words of output. The proxy answers those on a shrunk request instead of
forwarding them whole — each one used to bill a full prefix read, write a
cache tail, and leave a stored prefix that made the next real turn look like a
recache.

| Metric | What it shows |
|---|---|
| `proxy_sidecar_total{kind}` | Diverted sidecars. `kind="describe_action"` is the healthy path; `kind="fallback"` counted the ones whose shrunk request failed and were sent whole after all. |

```promql
# Fallback share — should stay near zero
rate(proxy_sidecar_total{kind="fallback"}[5m])
  / clamp_min(rate(proxy_sidecar_total[5m]), 1)
```

A rising fallback ratio means the sidecar model is unreachable or refusing the
rewritten body. A recache spike on turns *after* sidecar traffic is the symptom
this panel explains.

---

## Traffic & health panel

| Metric | What it shows |
|---|---|
| `headroom_requests_total` | Requests handled. Unlabelled. |
| `headroom_requests_by_provider{provider}` | Traffic split by provider. |
| `headroom_requests_by_model{model}` | Traffic split by model. Capped at 1024 distinct values; overflow lands in `model="other"`. |
| `headroom_requests_failed_total{provider}` | Rejected turns (`>= 400`), by provider. |
| `headroom_requests_rate_limited_total{source}` | 429s. `source="headroom"` is our own limiter; `source="upstream"` is the provider refusing. |
| `headroom_conversation_concurrency_sheds_total` | Turns shed by the per-conversation fan-out cap before anything was forwarded. These cost nothing — the client retries against a committed prefix. Zero until the cap is configured and tripped. |
| `headroom_compression_failed_total{reason}` | Fail-open compression failures — `timeout` or `error`. Traffic keeps flowing but savings quietly stop. **Worth an alert.** |
| `headroom_compression_quarantine_total{event}` | **Always zero on Rust, by design.** Quarantine is a workaround for a thread-pool constraint this runtime does not have (see `compression_quarantine.rs`); the family is registered for scrape-shape parity only. Alert on `headroom_compression_failed_total` instead. |
| `headroom_inbound_requests_active` | In-flight requests, gauge. Counts all HTTP including `/metrics`. |
| `headroom_active_ws_sessions` / `headroom_active_relay_tasks` | Live Codex WebSocket sessions and relay tasks, gauges. |
| `proxy_upstream_responses_total` + `proxy_upstream_rejections_total{status}` | Every upstream response and the non-2xx subset by status. The ratio is the panel — a count without a denominator is what let a 22.5%-refused defect hide for a day. 429s are counted but never escalated (provider throttling a healthy proxy); 5xx is counted, not escalated. |
| `proxy_upstream_retries_total{path,reason}` + `proxy_upstream_retries_exhausted_total{path,reason}` | Re-sends after transient failure (`status_429/529/5xx`, `transport`, `in_band_sse`) and the turns the loop gave up on. Retries re-bill input tokens. Defaults: 3 attempts, 6 for overload, 1s base backoff clamped to 30s (`--retry-max-attempts`, `--retry-overload-max-attempts`, `--retry-base-delay-ms`, `--retry-max-delay-ms`). |
| `proxy_stream_incomplete_total{provider}` | Streams that ended without their terminal event. Those turns are **dropped from the cost and savings books** rather than booked partial — a rising rate means the books are getting less complete, not that nothing happened. |
| `proxy_passthrough_bytes_modified_total{path}` | Bytes mutated on a path that promised byte-equal passthrough. **Must stay 0** — the cache-safety alarm. |

```promql
# Upstream rejection rate (429s excluded at the source: only 4xx-other-than-429 feeds the window)
sum(rate(proxy_upstream_rejections_total[5m]))
  / clamp_min(sum(rate(proxy_upstream_responses_total[5m])), 1)

# Savings silently stopped
sum by (reason) (rate(headroom_compression_failed_total[5m]))

# Retries that did not save the turn
sum by (path, reason) (rate(proxy_upstream_retries_exhausted_total[5m]))

# Cache-safety alarm — must stay 0
sum(rate(proxy_passthrough_bytes_modified_total{path!="__init__"}[5m]))
```

!!! warning "Alert thresholds, with the sizing behind them"
    The proxy itself escalates when 4xx-other-than-429 hits **10% over a
    rolling window of 50 responses with ≥ 3 rejections** (5-minute cooldown;
    `upstream_health.rs`). Mirror that in the dashboard rather than inventing a
    tighter one — below 3 rejections the rate is noise. `abandoned_requests_total`
    (in `/cache-health`) is a seam to look at, not an alert on its own: an
    upstream 429 bills nothing, so zero is the only value that needs no
    explanation.

---

## Subscription / rate-limit panel

API-key traffic reports remaining budgets; subscription/OAuth traffic reports
consumed-fraction windows. Both are gauges — don't `rate()` them.

| Metric | What it shows |
|---|---|
| `proxy_rate_limit_remaining_requests{provider}` / `..._tokens` / `..._input_tokens` / `..._output_tokens` | Last-seen upstream remaining budgets (input/output split is Anthropic-only). Smaller = closer to throttle. |
| `proxy_ratelimit_unified_utilization{window}` | Consumed fraction [0,1] of a subscription window (`5h`, `7d`, per-model like `7d_sonnet`). **`1 − value` is the remaining headroom** — the number to plot. |
| `proxy_ratelimit_unified_reset_seconds{window}` | Unix epoch of the window reset. |
| `proxy_ratelimit_unified_throttled{window}` | 1 when a window's status is anything but `allowed`. Boolean alarm signal. |
| `proxy_ratelimit_unified_fallback_percentage` | Share of traffic upstream steers to fallback capacity [0,1]. No window label. |
| `proxy_service_tier_count_total{tier}` / `proxy_response_status_count_total{status}` | OpenAI Responses tier distribution and terminal-status distribution. |

```promql
# Subscription headroom (what to plot)
1 - proxy_ratelimit_unified_utilization{window="5h"}

# Throttled windows — alert on 1
proxy_ratelimit_unified_throttled{window!="__init__"}
```

---

## Compression internals

| Metric | What it shows |
|---|---|
| `proxy_compression_ratio_by_strategy{strategy,content_type}` | `compressed / original` per shrunk block (in [0,1); smaller = better). Each strategy reports its own before/after. Buckets tighten near 0. |
| `proxy_compression_rejected_by_token_check_total{strategy}` | Compressor ran but the tokenizer check said the output was not smaller — original kept. High rate = the compressor needs tuning. |
| `proxy_compression_declined_no_shrink_total{strategy}` | Dispatcher declined before the tokenizer because the output was not smaller in bytes. Read against accepted volume: declines rising while accepted holds steady = the size gate absorbing waste; both falling = declining work that pays. |
| `headroom_compressions_by_strategy_total{strategy}` | Pipeline executions per strategy. |
| `headroom_kompress_size_gate_total{outcome}` | Size-gate decisions; `within` counts a gate pass, not that ML compression ran. |
| `headroom_waste_signal_tokens_total{signal}` | Wasteful patterns *detected* in the input (`json_bloat`, `base64`, `repetition`, `reread`…). Diagnosis, **not savings** — never add it to the hero number. |

```promql
# Per-strategy compression value at p50 / p95
histogram_quantile(0.50, sum by (strategy, le) (rate(proxy_compression_ratio_by_strategy_bucket{strategy!="__init__"}[1h])))
histogram_quantile(0.95, sum by (strategy, le) (rate(proxy_compression_ratio_by_strategy_bucket{strategy!="__init__"}[1h])))

# Share of runs the size gate absorbed, per strategy
sum by (strategy) (rate(proxy_compression_declined_no_shrink_total{strategy!="__init__"}[1h]))

# The control: accepted compression volume must not fall with it
sum by (strategy) (rate(proxy_compression_ratio_by_strategy_sum{strategy!="__init__"}[1h]))
```

---

## Five things that will break a dashboard

1. **Only the ledger survives a restart.** Every `/metrics` family is in-memory
   and resets to zero when the proxy restarts. Durability lives in the savings
   ledger on disk (`HEADROOM_WORKSPACE_DIR` on a persistent volume — otherwise
   it resets on every deploy), surfaced via `/stats` (`lifetime_metrics`,
   `savings_verdict`, `wire_verdict`), not via `/metrics`.

2. **Truncated streams leave the books.** `proxy_stream_incomplete_total` counts
   turns dropped from the cost and savings books because the stream ended
   without its terminal event. During provider trouble, savings rates look
   artificially clean while throughput falls — check this counter before
   believing a calm savings panel.

3. **`/metrics` needs auth if you set a proxy token.** With
   `HEADROOM_PROXY_TOKEN` set, any non-loopback scraper must send
   `Authorization: Bearer <token>`. Loopback is always exempt.

4. **Label vocabularies are bounded — read "other" as a signal.** `model`
   collapses past 1024 distinct values and `service_tier` buckets anything
   outside the spec into `other`. A growing `other` series means a buggy or
   hostile client is spraying values, not that traffic diversified.

5. **Quarantine and streaming-timer folklore.** `headroom_compression_quarantine_total`
   is permanently zero on Rust (see the traffic panel) — do not build an alert
   on it. And overhead/TTFB `_count` series are smaller than the latency count
   by construction (only sampled when > 0); always divide each `_sum` by its
   own `_count`.

---

## Python-mirror-only telemetry (not on the Rust proxy)

!!! note "OTel lives in `upstream-python/` only"
    Verified: `crates/` contains no OTel exporter — `HEADROOM_OTEL_*` and the
    dotted `headroom.proxy.*` family (`headroom.proxy.tokens.saved`,
    `headroom.proxy.savings.usd`, `headroom.compression.*`,
    `headroom.subscription.*`, `headroom_tokens_saved_by...` OTel twins) exist
    only in `upstream-python/headroom/observability/metrics.py`, and the
    `headroom_persistent_savings_*` Prometheus families only in
    `upstream-python/headroom/proxy/prometheus_metrics.py`. Likewise
    `headroom_savings_attributed_*` has no Rust emit site. If you are
    dashboarding the Rust proxy, every name in this section is a miss — use the
    Rust-native replacements in the panels above (`proxy_ratelimit_unified_*`
    for subscription windows, `/stats` for lifetime savings). If you are
    working on the Python mirror, that file is the reference.
