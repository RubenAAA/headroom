# Learning: compression ~1.5%, re-cache 38% — measured bill split

- **Source:** `docs/notes/proxy-experiments-2026-08.md` §22 (1,247 booked turns)
- **Claim:** 3.1% wire bytes removed on a 91%-cached workload ⇒ ~1.5% bill impact at best; cache writes are 55% of the bill; 174 re-cache events = 38.1% of input bill. Effort on compression ratios is effort on the 3%.


## Detail

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 22 — What the proxy actually costs and saves, measured

2026-08-09, 1247 booked turns, all of them `all_messages`. There is no period
in the log with compression off, so this is not the A/B — but it answers most of
the question without one.

### Compression is not where the money is

Client sent 485.8 MB, the proxy forwarded 470.6 MB: **3.1% removed**.

| billed input | tokens | share | rate |
| --- | --- | --- | --- |
| fresh input | 55,096 | 0.0% | 1.00x |
| cache read | 128,907,409 | 91.1% | 0.10x |
| cache write | 12,575,706 | 8.9% | 1.25x |

91% of input is served from cache, so 91% of anything compression removes would
have been billed at a tenth. Upper bound on compression's contribution is
roughly 1.5% of the bill, and that ignores any cache write it causes by moving
bytes.

### Cache writes are 55% of the bill

12.6M write tokens at 1.25x is 15.7M fresh-equivalents out of a 28.7M total,
from 8.9% of the tokens.

### 38% of the bill is re-caching what was already cached

174 re-cache events today, 8,733,092 wasted tokens — 69.4% of all cache-write
volume, 10.9M fresh-equivalents, **38.1% of the input bill**. `wasted_tokens` is
`min(expected_read - actual_read, cache_creation)`, so it is an upper bound: it
assumes the prefix should have been readable.

| replay outcome | events | wasted |
| --- | --- | --- |
| `prefix_content_diverged` | 107 | 6,339,455 |
| replay applied | 65 | 2,337,405 |
| other | 2 | 56,232 |

Breaking the diverged bucket down by what actually changed. Restricted to the
106 events carrying the full divergence diagnostics — the proxy was rebuilt
several times today and earlier events lack those fields, so including them
invents a phantom class:

| class | events | wasted | share of diverged | median idx |
| --- | --- | --- | --- | --- |
| client **dropped** text block(s) | 95 | 4,353,443 | 78.6% | 3 |
| client added block(s) | 8 | 1,098,858 | 19.8% | 171 |
| thinking blocks | 2 | 46,384 | 0.8% | 4 |

So the price of the behaviour trade item 20 left open is **19.0% of the input
bill** — 4.35M wasted tokens, 5.44M fresh-equivalents. An earlier pass here said
27.6%, reached by attributing the whole diverged bucket to one class.

Item 18's thinking-block divergences are confirmed free: 2 events and 46K
tokens, because those attempts are rejected and never billed.

On these turns `expected_cache_read` totals 6,690,953 against an actual
3,092,273, so the prefix still half-hits — the break is real but not total.

### Reading of the original question

"Is the proxy saving tokens?" Compression: almost nothing, ~1.5% at best. Cache
stabilisation: that is the whole product, and 38% of the bill is still on the
table. Effort spent on compression ratios is effort spent on the 3%.

### Scoping trap, twice

`proxy.log` is never rotated. The first pass at this summed `cache_recache_observed`
across months against one day of `turn_cost_ledger` and produced waste at 123.6%
of the bill. Any figure here must be scoped by date.
