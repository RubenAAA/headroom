# Learning: the classifier's hiding places were checked, not assumed

- **Source:** `docs/notes/recache-classification.md` (bias section, 2026-08-22)
- **Claim:** deliberately biased toward flagging (a look is cheaper than a
  hidden fix), then each hiding place measured shut: TTL path fixed via
  `with_cache_ttl` (had hidden ~3% of creation); 64-token slack hit 0
  shortfall pairs; `min(shortfall, write)` clamp is right (12/20 clamped
  events' extra was never billed); booking gap refuted 577/577. Deliberate
  bias + measured closure beats silent filing.


## Detail

*moved from `docs/notes/recache-classification.md`*

## Is the classifier calling things waste that are not? Mostly the opposite

Worth stating the bias deliberately, because it decides which way to fix
anything found here: flagging a turn that turns out to be benign costs a
look, while filing a fixable loss as expected hides it for good. Prefer the
first. Every entry below was checked in that direction.

**The TTL path was already fixed and is correct now.** `classify_turn` files
a bust as `TtlExpiry` when the gap exceeds the TTL it is told about. Left at
the 5-minute default while the proxy pins 1h, every bust in the 5m–1h gap
reads as a legitimate expiry — `proxy.rs:580-582` records that this hid about
3% of daily creation. `with_cache_ttl(observed_cache_ttl)` now passes the
pinned value, and the running proxy has `--force-1h-cache-ttl true`.

**The slack early return hides nothing here. REFUTED.** `classify_turn`
returns `Healthy` when the re-write is under `RECACHE_SLACK_TOKENS` (64),
on the reasoning that nothing meaningful was billed. That could in principle
swallow a small loss repeated every turn. Measured across this window: **0
turn-pairs** hit it with a read shortfall above the slack. Not a leak.

**The clamp is right, not conservative.** `wasted = min(shortfall, write)`
caps reported waste at what was actually paid to re-create. Without it a
conversation that simply got shorter would report the entire missing read as
waste. Twelve of the twenty events sit on this clamp, which means their true
shortfall was larger — but the extra was never billed, so it is not waste.

**Turns escaping accounting. REFUTED.** If a turn never reached `complete()`
its loss would be invisible. Measured: 577 requests forwarded, 577 booked,
2 unbooked. Booking is not the leak.


## Detail

*moved from `docs/notes/recache-classification.md`*

### Refuted here

- **TTL.** 217 of 218 sequential shortfalls followed a 1h write, with gaps in
  seconds. Not expiry.
- **The proxy rewriting the front of history.** `messages_rewritten` touches
  index <=3 on 95.4% of shortfalls and 95.2% of healthy turns — identical, so
  it discriminates nothing. Rewriting the front is universal and normally
  harmless.
- **Context offload volume.** Lower on the residue (7 blocks) than on healthy
  turns (12), the wrong direction for a cause.
