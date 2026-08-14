# Measurement — is the proxy paying for itself?

A compression proxy can report a healthy ratio while costing more than it
saves. Every token compression removes is a token saved, but every byte the
proxy moves inside a prompt-cache prefix costs a full re-creation of everything
after it — and on a subscription, cache reads are free while creations are not.
`docs/subscription-optimization.md` has two measured configurations that saved
tokens on paper and cost **−31.3%** and **−54.1%** in practice.

So "did we compress" is the wrong question. This page is the right one, and
where to read the answer.

## Measurement surfaces

| surface | scope | survives restart |
|---|---|---|
| statusline (`scripts/statusline-cache-health.sh`) | last bust, last 50 requests | no |
| `GET /cache-health` | in-process watchdog | no |
| `headroom savings` | successful compression events; selected input only | yes |
| `GET /stats` → `lifetime_metrics`, `savings_verdict` | durable totals | **yes** |
| `GET /metrics` | Prometheus, see `docs/observability.md` | scrape-dependent |

The first two reset when the proxy restarts. Only `/stats` gives you a baseline
to compare a config change against.

`headroom savings` answers a narrower question: when a transform produced a
measured saving, how much of the input selected for that transform did it
remove? Its ledger omits zero-saving turns and never includes the untouched
prompt, so its percentage is not a share of all provider input. The CLI labels
the denominator as `selected tokens` for that reason.

Its dollar value is an estimate at the saved tokens' measured cache placement.
New proxy ledger rows record `cost_basis` as `fresh_input` or `cache_read`;
legacy rows without that field used the fresh-input rate and can overstate the
historical aggregate. They cannot be repriced after the fact because their
placement was not recorded.

## The one number

```
curl -s localhost:8787/stats | jq .savings_verdict
```

```json
{
  "net_tokens_saved": 18400,
  "tokens_saved_by_compression": 21000,
  "tokens_lost_to_cache_busts": 2600,
  "bust_count": 3,
  "prefix_change_misses": 3,
  "ttl_expiry_misses": 41,
  "unknown_misses": 2,
  "cache_read_tokens": 4210000,
  "cache_write_tokens": 190000,
  "verdict": "saving"
}
```

`net_tokens_saved` is `tokens_saved_by_compression` minus
`tokens_lost_to_cache_busts`: what we removed, less what we made the provider
rebuild. Negative means the proxy is losing. The raw inputs sit beside it so the
number can be checked rather than believed.

## The number that crosses the boundary

Everything in `savings_verdict` measures the proxy against itself: tokens a
transform removed, bytes moved on the `tools`+`system` axis. None of it can say
the provider saw less, because the provider's own `usage` is the only authority
on that.

```
curl -s localhost:8787/stats | jq .wire_verdict
```

```json
{
  "bytes_in": 184320000,
  "bytes_out": 97117000,
  "bytes_saved": 87203000,
  "bytes_saved_percent": 47.3,
  "provider_input_tokens": 1996000,
  "provider_cache_read_tokens": 70037000,
  "provider_cache_write_tokens": 4090000,
  "provider_billed_tokens": 6086000,
  "provider_cache_hit_percent": 92.0,
  "bytes_per_billed_token": 15.9,
  "measured_requests": 4351
}
```

Both halves come from the same requests: the body as received, the body put on
the wire, and the usage Anthropic reported for it. A request with no usage —
a stream that died before it arrived — is left out of both, so the ratio never
counts bytes against absent tokens.

`bytes_per_billed_token` is the reconciliation. Cache reads are free on a
subscription, so `provider_billed_tokens` counts uncached input plus creation
only. Watch it across a config change:

- **Bytes fall, ratio steady** — the saving converted. Fewer bytes really did
  mean fewer billed tokens.
- **Bytes fall, ratio falls with them** — the proxy is stripping bytes the
  tokenizer barely charged for. Motion, not saving.
- **Bytes fall, billed tokens rise** — compression is busting the cache. Read
  `prefix_change_misses` next.

The per-request pair is also logged, so a single turn can be checked:

```
grep outbound_body_bytes "$PROXY_LOG" \
  | jq -s 'map(.fields)|{reqs:length,in:(map(.bytes_in)|add),out:(map(.bytes_out)|add)}'
```

The miss buckets say whose fault a rebuild was:

- **`prefix_change`** — the drift detector saw bytes move inside the cached
  prefix. This is us, or the client. It is the one to act on.
- **`ttl_expiry`** — the idle gap outran the cache TTL. Time passing, nothing
  to do with the proxy. Expect this to grow during a normal working day.
- **`unknown`** — a rebuild with no drift to blame, usually a session reset
  (`/clear`, a subagent finishing). Counted, not charged as waste.

## Healthy looks like this

- Statusline reads `cache ✓ 90-99%` and shows no `⚠ recache` during steady work.
- `net_tokens_saved` positive and growing.
- `prefix_change_misses` flat. Not zero forever — a late MCP server or a client
  restart will bump it — but flat between them.
- `ttl_expiry_misses` growing slowly. Benign.
- `cache_read_tokens` an order of magnitude above `cache_write_tokens`.
- `waste_signals.reread_compressed` flat.

## Failure modes

### The prefix cache is being busted

**Signature:** `prefix_change_misses` climbing, `tokens_lost_to_cache_busts`
climbing, `net_tokens_saved` falling or negative. Statusline shows `⚠ recache`
repeatedly, naming `system`, `tools` or `early_messages`.

The statusline names the drifted dimension, which is usually enough to find it:

```
⚠ recache 42s ago: tools, ~104K tok wasted
```

`tools` most often means a tool set that changed mid-session — an MCP server
finishing its handshake late. `system` or `early_messages` during steady work is
more serious: something is rewriting content the provider had already cached.

**Revert:** `--compression-mode off`. That is the configuration the proxy ran
for months, and it is a proven no-op.

### The proxy is doing nothing

**Signature:** `verdict` reads `"no data yet"` after real traffic, or
`tokens_saved_by_compression` stays 0 across many requests.

Check the compression decision log — `reason=mode_off` on every request means
compression is off:

```
grep 'anthropic compression decision' "$PROXY_LOG" \
  | grep -o 'reason=[a-z_]*' | sort | uniq -c
```

Two things to check in order: the proxy binary is the one you think it is (a
running proxy is reused, not restarted — see `--context` in the launcher), and
`--compression-mode` resolves to something other than `off`.

### Compression is too lossy

**Signature:** `waste_signals.reread_compressed` climbing while
`net_tokens_saved` looks healthy.

```
curl -s localhost:8787/stats | jq '.lifetime_metrics.waste_signals'
```

This is the cost per-request token counts cannot see. The model got a summary,
could not work with it, and the client resent the full content — extra turns,
not extra tokens on any one turn. `net_tokens_saved` will call that a win.

`reread` is every re-read; `reread_compressed` is the subset whose first serve
we had compressed away. Only the second is our doing.

**Mitigations:** `--exclude-tools` keeps named tools' results away from the
lossy compressors *and* from ctx offload, and defaults to the file and search
tools for this reason. It used to bind only the compressors, so an excluded
tool's oversized output was still replaced by an offload digest one stage
earlier; with both honouring it, `Read`/`Grep`/`Edit` results stay whole by
default and offload volume drops accordingly.
`--ctx-offload` gives compression a CCR store, so a `headroom_retrieve` call can
recover an original instead of the client resending it.

### Offload is putting back most of what it took out

**Signature:** `ctx_proactive_expansion_bytes_total` close to
`ctx_offloaded_bytes_total` on `/metrics`, or the same pair on
`/ctx/stats`.

```
curl -s localhost:8787/ctx/stats | jq '{offloaded_bytes, proactive_expansion_bytes}'
```

Offload replaces a tool result with a digest; CCR proactive expansion appends
previously-offloaded content back onto the latest user turn when the query
looks like it needs it. Read the offload figure alone and every turn looks like
a saving. The difference between the two is the saving.

Expansion is on by default and capped by `--ccr-max-proactive-expansions`
(default 2). Lower the cap, or set `--ccr-proactive-expansion false`, if the
gap is not paying for itself.

### Cache writes are landing on the wrong TTL

**Signature:** `lifetime_metrics.prefix_cache.ttl_1h_percent` near zero while
`ttl_expiry_misses` climbs fast.

Idle gaps are outrunning the 5-minute default. `--force-1h-cache-ttl true`
pins markers to an hour; read the B1 section of
`docs/subscription-optimization.md` first, because the 5-minute cache refreshes
on every use at no cost and the win is narrower than it looks.

## What none of this can see

- **A model that reasons badly off a summary and never asks again.**
  `reread_compressed` only counts re-reads the client actually sends.
- **Whether a compressed tool result was still correct.** Nothing here checks
  meaning, only tokens.
- **Anything on a passthrough request.** The transforms and most counters live
  on the buffered branch; a proxy that is not intercepting measures very little.
- **Cost in dollars, from `proxy_savings.json`.** Its `total_input_cost_usd`
  does not reconcile against `total_input_tokens`. Trust the token counts.
- **Turns that were cut off mid-stream.** Their usage never arrives, so they
  are left out of the books entirely rather than booked at a partial figure.
  `proxy_stream_incomplete_total` says how many are missing.

## Logs worth grepping

| event | level | meaning |
|---|---|---|
| `cache_recache_observed` | WARN (`event_kind="drift"`) | a structural bust — bytes moved |
| `cache_recache_observed` | INFO (`event_kind="expected"`) | rebuild with no drift to blame |
| `cache_recache_ttl_expiry` | DEBUG | idle gap beat the TTL |
| `cache_drift_observed` | — | the hot zone changed between turns |
| `anthropic compression decision` | INFO | carries `decision` and `reason` |
| `outbound_body_bytes` | INFO | the final wire size, after every transform |
| `ctx_inject_row_miss` | WARN | a conversation lost its persisted injection row |
| `cache_stable_tool_order` | DEBUG | B2 replayed the previous tool order |
| `stream_incomplete` | WARN | stream ended before `message_stop`; carries the partial token counts for a turn that is deliberately not in the books |

## Measuring a change properly

Do not A/B by running with a setting on for a day and comparing to yesterday.
`docs/subscription-optimization.md` documents its own first harness being
invalid: a passthrough sanity run showed **+56%** purely because the OFF pass
warmed Anthropic's cache for the ON pass.

For a real number, capture a session and replay it offline:

```
HEADROOM_CAPTURE_DIR=/path/to/corpus headroom-proxy ... # one session of real work
cargo run -p headroom-proxy --bin offload_sim -- /path/to/corpus
```

Capture writes request bodies only — never headers, so no credential can reach
the corpus — and it is off unless the variable is set. Treat the result as
private conversation data and delete it after.
