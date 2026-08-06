# Measurement — is the proxy paying for itself?

A compression proxy can report a healthy ratio while costing more than it
saves. Every token compression removes is a token saved, but every byte the
proxy moves inside a prompt-cache prefix costs a full re-creation of everything
after it — and on a subscription, cache reads are free while creations are not.
`docs/subscription-optimization.md` has two measured configurations that saved
tokens on paper and cost **−31.3%** and **−54.1%** in practice.

So "did we compress" is the wrong question. This page is the right one, and
where to read the answer.

## Three surfaces

| surface | scope | survives restart |
|---|---|---|
| statusline (`scripts/statusline-cache-health.sh`) | last bust, last 50 requests | no |
| `GET /cache-health` | in-process watchdog | no |
| `GET /stats` → `lifetime_metrics`, `savings_verdict` | durable totals | **yes** |
| `GET /metrics` | Prometheus, see `docs/observability.md` | scrape-dependent |

The first two reset when the proxy restarts. Only `/stats` gives you a baseline
to compare a config change against.

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
lossy compressors, and defaults to the file and search tools for this reason.
`--ctx-offload` gives compression a CCR store, so a `headroom_retrieve` call can
recover an original instead of the client resending it.

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

## Logs worth grepping

| event | level | meaning |
|---|---|---|
| `cache_recache_observed` | WARN (`event_kind="drift"`) | a structural bust — bytes moved |
| `cache_recache_observed` | INFO (`event_kind="expected"`) | rebuild with no drift to blame |
| `cache_recache_ttl_expiry` | DEBUG | idle gap beat the TTL |
| `cache_drift_observed` | — | the hot zone changed between turns |
| `anthropic compression decision` | INFO | carries `decision` and `reason` |
| `cache_stable_tool_order` | DEBUG | B2 replayed the previous tool order |

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
