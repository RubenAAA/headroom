# Headroom — Subscription Usage Optimization

**Goal:** upgrade headroom so it measurably *reduces* a Claude Code **subscription**
user's consumption (the `anthropic-ratelimit-unified-*` windows), proven
lever-by-lever with data-backed A/B results on real captured traffic.

Headroom was built for **API** usage (pay-per-token, no client cache). On a
**subscription** with Claude Code's aggressive prompt caching, naive compression
*busts the cache* and **increases** usage. This doc tracks the levers that
actually work, each validated against real captures.

## How Claude Code uses the prompt cache (from client source)

Source: `meta/claude-code-main/services/api/claude.ts`, `promptCacheBreakDetection.ts`.

A `/v1/messages` request carries up to **4 `cache_control` (ephemeral) breakpoints**:
- up to **2 on `system`**
- **1 on `tools`** (the whole tool array is one cached segment)
- exactly **1 on the last message** (`addCacheBreakpoints`, "exactly one
  message-level marker per request"; 2nd-to-last for fire-and-forget forks)

Because the message marker rides the *last* message, **the entire history is the
cached prefix every turn**. Anything before a marker that changes bytes → cache
miss + cascade for everything after it.

**Cost model — confirmed from Anthropic docs** ([rate-limits](https://docs.claude.com/en/api/rate-limits)):
verbatim, *"cached input tokens **do not count towards rate limits** and are billed
at a reduced rate (10% of base input token price)."* So for the rate-limit / usage
window: **`cache_read` = 0× (free)**, **`cache_creation` = full 1×** (billed 1.25×
5min / 2× 1h), fresh `input`/`output` = 1×. Therefore:

> **subscription usage ≈ cache_creation + fresh_input + output** (cache reads are free)

The ONLY winning levers cut **cache creation without busting existing cache**:
tool pruning (A4), `all_messages` compress-from-birth (A2), and Class-B bust
prevention (B1/B2). Anything that busts the prefix cache converts *free* reads into
*full-price* creates — strictly worse. (The doc is the API rate-limit rule; the
subscription `unified` window is inferred to mirror it — confirm via
`anthropic-ratelimit-unified-*` headers on a clean 200.)

## Payload composition (39 real captures)

| segment | share | headroom-addressable? |
|---|---|---|
| tools (schemas) | 46.4% | only via pruning (risky) |
| tool_result (file reads) | 29.9% | yes — compress + clear |
| message text | 12.8% | yes |
| thinking | 6.5% | yes — clear_thinking |
| system | 2.8% | no (cached prefix) |
| tool_use | 1.5% | no |

## Levers

### Class A — shrink what's sent
| # | lever | mechanism | risk | status |
|---|---|---|---|---|
| A1 | `clear_tool_uses_20250919` | inject Anthropic context-editing; drops old tool results server-side past a token trigger, keeps N recent | low (Anthropic-managed, placeholders) | building |
| A2 | `all_messages` compression | compress tool_result/text in ALL messages deterministically (cache-stable) | medium (lossy; needs CCR retrieval) | built, deployed, +4.3% |
| A3 | `clear_thinking_20251015` | inject context-editing; drop old thinking blocks | low | building (same fn as A1) |
| A4 | tool pruning | drop never-used tools (46% mass) via operator allowlist / MCP-server drop-list; deterministic → cache-safe | low if operator-configured (never auto) | **BUILT + WIRED** (+30.6% measured ceiling) |

### Class B — prevent cache misses (lossless)
| # | lever | mechanism | risk | status |
|---|---|---|---|---|
| B1 | force 1h cache TTL | pin `ttl:'1h'` so the prefix survives >5min gaps instead of full re-creation | low | not started |
| B2 | cache-bust prevention | pin system/tools/cache_control/betas/effort stable so client churn doesn't re-create the ~180k prefix | low (lossless) | not started |

## Results (data-backed, real captures, 8-request conversation)

A/B = OFF (direct to Anthropic) vs ON (through proxy / with feature). Weighted
consumption = `input + cache_creation + 0.1·cache_read + output`. Within-run
ON−OFF delta is the clean signal (absolute numbers drift via cache warming).

| config | weighted ON vs OFF | verdict |
|---|---|---|
| headroom stock (live_zone, auto-frozen off, kompress on) | **−31.3%** | harmful (cache cascade) |
| live_zone, auto-frozen on, kompress on | −9.4% | still bad |
| live_zone, auto-frozen on, kompress off | 0.0% | neutral no-op |
| live_zone, auto-frozen off, kompress off (compress prefix) | **−54.1%** | worst (position-keyed cascade) |
| **all_messages, kompress off (A2)** | **+4.3%** | first real win; cache-stable |
| **clear_tool_uses keep2/20k (A1, idealized warm)** | **+28%** | big; some warming inflation |
| economics ceiling (tool_results @ 50% consistent) | +34.8% | upper bound for A2 at 2× ratio |

## Sequential validation plan (each step measured, confirm/deny)

1. proxy `mode=off` → expect ≈0% (sanity: passthrough doesn't change usage)
2. `+ A1` clear_tool_uses (keep 6 / trigger 60k, conservative) → measure
3. `+ A3` clear_thinking → measure
4. `+ A2` all_messages compression (full stack) → measure
5. `+ B1` force 1h TTL → measure
6. `+ B2` bust-prevention → measure

Results appended below as each is validated.

### Methodology (corrected)

The original OFF-then-ON harness was **invalid**: a passthrough sanity run showed
**+56%** purely because the OFF pass warms Anthropic's cache for the ON pass
(5min/1h TTL). Corrected method: **one cold-start replay per config**, isolated by
a unique nonce. The nonce MUST perturb the **first tool's description** — Anthropic
caches in order `tools → system → messages`, so a system-only nonce leaves the
dominant tools cache warm (verified). Each run is cold at turn 0 and self-warms
turn-to-turn like real usage. Compare weighted TOTALs across configs.

Validation: two baselines with different nonces → **byte-identical 469,624**
(deterministic). Conversation = `req-2..15` (one conversation, 14 turns, grows to
~233k-token context). Through the live proxy on the real Max subscription.

### Run log (14-turn conversation, cold-start, weighted total)

| # | config | weighted total | vs baseline |
|---|---|---|---|
| baseline | proxy passthrough (mode=off) | 469,624 | — |
| A1 | clear_tool_uses keep6/trigger60k (no clear_at_least) | 660,246 | **−40.6% (WORSE)** |
| A1' | clear_tool_uses keep6/trigger120k/clear_at_least80k | ~470,655 | ~0% (neutral) |
| A2 | **all_messages compression (kompress off)** | **446,757** | **+4.9% (real win)** |
| A4 | **tool pruning to called-set (81→2 tools)** | **325,769** | **+30.6% (BIGGEST win)** |

**A4 (tool pruning) — the biggest lever by far, and robust to the weighting debate.**
This 14-turn conversation DEFINED 81 tools (130KB) but CALLED only 2 (Read, Bash).
Pruning to the called-set: weighted 469,643 → 325,769 = **+30.6%**. Crucially it cut
BOTH buckets — cache_creation 234,832→172,278 (−62k) AND cache_read 2,344,888→1,531,686
(−813k) — so it wins under ANY weighting (raw-token total −34%, weighted-0.1× −30.6%).
88.5% of tool mass was unused; almost all of it whole MCP servers (chrome ~25 tools,
context-mode ~12, tmux ~15, perplexity, qwen). LOSSLESS + cache-stable (deterministic
pruned set) → fits even headroom's conservative subscription policy (auth-modes.md:
"lossless-only, no auto cache_control"). PRODUCTION-SAFE design = user-configured
MCP-server / tool allowlist, NOT auto-prune (can't drop a tool a future turn needs).
325,769 is the CEILING (this convo used 2 tools); realistic curated keep-set captures
a large fraction. Harness: `prune_run.py`. Direct-to-Anthropic, cold, nonce-isolated.

**WEIGHTING IS UNVERIFIED — the whole compression verdict may be sign-inverted.**
All "weighted" numbers assume `cache_read = 0.1×` (API PRICING). We never confirmed the
**subscription quota** (`anthropic-ratelimit-unified-*`) uses that discount. If it counts
cache reads closer to 1×, the −31% compression run flips to −4.6% BETTER (it sent 54,844
FEWER total tokens; only looked bad because we discounted the reads it removed). Ground
truth = the unified rate-limit headers on a 200 response. Blocked 2026-06-20 by a 429
(the 28 benchmark replays exhausted a window; OAuth 429s carry NO ratelimit headers).
Resolve before trusting any compression verdict. NOTE: tool pruning is immune to this —
it wins in every bucket regardless of weighting.

**CLEAN CAMPAIGN CONCLUSION:** Only `all_messages` (+4.9%) genuinely saves on this
14-turn cold conversation. `clear_tool_uses` is neutral-at-best (harmful if naive) —
clearing trades 0.1× reads for 1× re-creates and needs long sessions to amortize.
Caveats: (1) 14-turn cold is a conservative lower bound — longer sessions favor both
levers; (2) the dominant cost is the **tools cache** (~72k read every turn + 81k cold
create ≈ 37% of total), which NO Class-A lever touches. The real frontier is **Class B
(cache-miss prevention)** — keep that 72k tools prefix from being re-created on gaps
(1h-TTL) and prevent client-induced busts. Untested here (replay has no >5min gaps),
but it attacks the 1× creation directly, which is the only thing that wins on cold-start.

**A1 finding:** naive `clear_tool_uses` THRASHES the cache. `keep:6` is a sliding
window on a growing conversation → the cleared prefix changes every turn → cache
re-creation every turn (cc balloons 83k/52k while cr pinned ~72k). The earlier
+28% was the warming artifact. Fix hypothesis: add `clear_at_least` + raise trigger
so it clears infrequently in big chunks and rides the cache between clears.
NOTE: all prior +X% (incl. all_messages +4.3%) used the biased harness — re-measuring all.

**A1 param sweep (clean, direct injection):** keep6/60k/none = 660,269 (−40%); keep6/trigger120k/clear_at_least80k = 470,655 (**neutral**); keep3/100k/clear_at_least60k = 494,051 (−5%). Best achievable = break-even. `clear_tool_uses` does NOT net-save on a 14-turn cold conversation (cleared content isn't re-read enough to amortize the re-creation; tools cache — the dominant ~72k/turn — is untouched). Would only help on long sessions. DENIED for this workload. Key lesson: levers that shrink 0.1× reads while adding 1× creates lose; only levers that cut CREATION (all_messages compress-from-birth, or Class B cache-miss prevention) can win on cold-start.

