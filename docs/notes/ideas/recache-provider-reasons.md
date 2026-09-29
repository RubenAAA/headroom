# Reopened: provider-side residue crossed the five-figure bar

> **Correction 2026-09-29 (read first).** The `unexplained_after_replay`
> residual below is mostly not lost cache. 12 of 12 events on 2026-09-29 came
> right after a turn that ran one server-side tool call
> (`tool_search_tool_regex`), against 22 such turns in 1,315. The provider
> sampled again inside that request and reported usage summed over the
> iterations: read 119,815 where the conversation ran 58k to 60k, with 2,636
> uncached input tokens. The next turn read normally (60,294) and was scored
> against the summed figure. Across 2026-09-24..29, 38 of 38 such
> `unexplained_after_replay` events (213,472 tokens) and 13 of 14
> `aftershock_of_continuation` events (135,691 tokens) read at or above the
> boundary of the turn *before* the server-tool turn. Fixed in the proxy:
> server-tool turns go through `UsageObserver::complete_summed`, which bills and
> ledgers the turn but neither scores it nor stores it as the baseline; each
> logs `usage_baseline_skipped`. What remains after that is the class without
> a server-tool turn before it (24 events, 456,445 tokens in the same window),
> which is the real residual to study. Check after a restart, counting
> rows after the restart time only:
>
> ```python
> # unexplained_after_replay events whose previous turn ran a server tool
> # (should go to ~0), and usage_baseline_skipped counts
> import json
> ev = skipped = 0
> for l in open("/home/ruben/headroom-proxy.log", errors="replace"):
>     if "cache_recache_observed" in l or "usage_baseline_skipped" in l:
>         fl = (json.loads(l).get("fields") or json.loads(l))
>         skipped += fl.get("event") == "usage_baseline_skipped"
>         ev += (fl.get("event") == "cache_recache_observed"
>                and fl.get("attribution_reason") == "unexplained_after_replay")
> print("unexplained", ev, "skipped baselines", skipped)
> ```
>
> **What the 24 leftover events are (2026-09-24..29 logs).** 9 events /
> 282,368 tokens are Codex-routed turns (`claude-codex-6-luna` through
> chatgpt.com), 09-25 10:19..10:51: OpenAI reports no write counter and reads
> in 1,024-token steps, and the read fell to the 15,104-token instructions
> prefix each time. The Anthropic-shaped detector scored them. The 09-25
> `concurrent_turn_in_flight` episode (18 events, 2,628,608 tokens, reads
> 15k to 24k) is the same route and the same collapse. Routed turns are 30 of
> 787 events but 2.91M of 5.02M wasted tokens in these logs. The route sends
> already carries a `prompt_cache_key` (the translator sets it to the raw
> `metadata.user_id`, one per session; the E4 `e4_skipped reason=auth_mode`
> skip only covers the synthesised key). 42 of 233 routed turns that day read
> under half their prompt, and the miss rate rises with the gap since the
> session's previous turn: about 9% under 10 s, 24% under 60 s, 86% between
> 120 s and 300 s. That points at short backend retention or routing, not a
> missing key. The key now equals the `session-id` header (see
> `enable-e4-cache-key-ab.md`); the lift is unmeasured. 14 events / 6,621 tokens are
> 350-1,000 token `missed_newest_write` races and are noise. One event is
> Anthropic-shaped: 09-28 16:30:41 UTC, read 182,333 -> 15,090 (the system and
> tools prefix), 167,456 tokens, idle 97 s, with system, tools, beta, auth,
> model, markers and the 256-message ladder all unchanged. That is the shape
> the docs give for changed thinking, tool_choice or speed settings, but no
> log line records them, so it is unproven. `turn_cache_fingerprint` now
> carries `request_params` (thinking, tool_choice, output_config,
> context_management, speed, service_tier, sampling); after a restart, diff it
> between a collapsed turn and the turn before.

- **Status:** open again 2026-09-29 — one residual recache reached 12,782
  tokens, crossing the explicit re-open threshold below.
- **Source:** `docs/notes/recache-classification.md`
- **Summary:** The 2026-09-29 live window has 9 `unexplained_after_replay`
  events / 50,968 wasted tokens. All nine have no logged content/head/beta/
  marker/model drift; `prefix_stable_msgs` equals `turn_msgs`, and the
  provider read positions are below the previous turn's read. Eight land at
  the older-generation boundary plus a small sliver; one is below the older
  boundary, which is unknown on that event. Seven carry
  `commit_race_suspect=true`, but the two without it have the same older
  snapshot shape. A 12,782-token event crossed the five-figure reopening bar.
  This reopens investigation, not the claim that a local stabilizer fix exists.
- **Value:** find whether routing, provider cache retention/replication, or a
  client-side timing change can prevent these older-snapshot reads without
  damaging the cache hit rate.
- **Next:** preserve this exact log window and compare the old-snapshot subset
  against any routing/cache-worker evidence available from the provider. Do
  not add a fixed delay or retry on the strength of the suspect flag alone.
  The complete event table and sanitized replay command are below.


## 2026-09-29 live audit — full recache inventory

**Scope.** Read `~/headroom-proxy.log` through 2026-09-29 09:51:00Z
(13:51:00 Asia/Yerevan). The log contains one proxy start in the local-day
window, 08:06:15Z (12:06:15 local, PID 926), and no later start by the audit
cutoff. The 15 recache events are 12:39:21–13:50:20 local. All numbers below
are from that fixed cutoff; the append-only log may have grown since.
Timestamps in the table are Asia/Yerevan. A fresh scan at 14:22 local found
no later recache event, so this fixed window covers all observed recaches
today through that time. No prompt bodies, raw message text, session keys, or
request identifiers were read or copied.

| Local time | Attribution / landing | Waste / write | Actual / expected read | Previous read / older boundary | Idle / race suspect | Other structural evidence |
|---|---|---:|---:|---:|---:|---|
| 12:39:21 | `unexplained_after_replay` / `provider_dropped_older_entry` | 350 / 350 | 45,737 / 91,474 | 49,207 / unknown | 9.25s / yes | no drift dimensions |
| 12:40:55 | `unexplained_after_replay` / `provider_between_entries` | 4,331 / 4,331 | 60,294 / 120,588 | 119,815 / 59,521 (+773) | 17.21s / yes | no drift dimensions |
| 13:07:16 | `unexplained_after_replay` / `provider_between_entries` | 12,782 / 12,782 | 193,784 / 387,568 | 386,592 / 192,808 (+976) | 14.80s / yes | no drift dimensions; five-figure threshold |
| 13:07:26 | `unexplained_after_replay` / `provider_between_entries` | 4,015 / 4,015 | 65,850 / 131,700 | 129,012 / 63,162 (+2,688) | 24.22s / no | no drift dimensions |
| 13:11:28 | `prefix_head_changed` | 537 / 2,532 | 272,008 / 272,545 | — | 45.89s / yes | system head + beta; 364 → 364 messages |
| 13:13:38 | `unexplained_after_replay` / `provider_between_entries` | 6,495 / 6,495 | 81,304 / 162,608 | 162,008 / 80,704 (+600) | 11.35s / yes | no drift dimensions |
| 13:22:39 | `prefix_content_diverged` | 210,937 / 210,937 | 27,736 / 265,375 | — | 266.71s / no | first difference at 68; 251 → 254 messages; early-message outbound drift |
| 13:24:36 | `aftershock_of_continuation` | 4,113 / 10,093 | 120,994 / 125,107 | — | 18.49s / yes | `origin=previous_turn` |
| 13:24:44 | `aftershock_of_continuation` | 3,127 / 3,127 | 120,994 / 131,087 | — | 8.58s / yes | `origin=previous_turn` |
| 13:29:42 | `prefix_head_changed` | 636 / 2,631 | 271,634 / 272,270 | — | 137.03s / no | system head + beta; 303 → 303 messages |
| 13:33:15 | `aftershock_of_continuation` | 1,574 / 1,574 | 143,777 / 154,304 | — | 7.20s / yes | `origin=previous_turn` |
| 13:40:32 | `unexplained_after_replay` / `provider_between_entries` | 4,868 / 4,868 | 53,682 / 107,364 | 105,564 / 51,882 (+1,800) | 26.67s / no | no drift dimensions |
| 13:45:47 | `unexplained_after_replay` / `provider_between_entries` | 4,386 / 4,386 | 95,628 / 286,742 | 286,240 / 95,126 (+502) | 5.03s / yes | no drift dimensions |
| 13:47:31 | `unexplained_after_replay` / `provider_between_entries` | 7,195 / 7,195 | 74,920 / 149,840 | 149,221 / 74,301 (+619) | 13.30s / yes | no drift dimensions |
| 13:50:20 | `unexplained_after_replay` / `provider_between_entries` | 6,546 / 6,546 | 127,768 / 255,536 | 254,155 / 126,387 (+1,381) | 14.04s / yes | no drift dimensions |

Totals: **15 events, 271,892 wasted tokens**. By attribution:

| Attribution | Events | Wasted tokens | Assessment |
|---|---:|---:|---|
| `unexplained_after_replay` | 9 | 50,968 | Provider-side residual; reopened below |
| `aftershock_of_continuation` | 3 | 8,814 | Proxy-created CCR continuation boundary; plausible policy experiment |
| `prefix_content_diverged` | 1 | 210,937 | Client-attributed prefix divergence; no safe replay fix identified |
| `prefix_head_changed` | 2 | 1,173 | Client system head changed; preserve the new request semantics |

The nine residual turns have `drift_dims=""`, `outbound_drift_dims=""`,
`head_moved=""`, `beta_changed=false`, `markers_changed=false` and
`model_changed=false`. For each, `prefix_stable_msgs == turn_msgs`; the
reported `matched_stream_msgs` is two messages smaller, a distinction retained
in the log rather than treated as proof that every tracker count is identical.
Their `scope` is `replayed_prefix` and `origin` is `unknown`. In all nine,
`actual_cache_read < previous_cache_read`. The eight
`between_entries` readings are exactly the older boundary plus 502–2,688
tokens, rather than a monotonic shortfall from the immediately previous
generation. Two events are not marked as commit races (13:07:26 and 13:40:32)
but still have that shape. This makes a commit-latency-only explanation
insufficient for this window. The seven race-suspect events remain
correlated evidence, not proof of a local race fix.

### Preventability and fix investigation

- **Provider residual (50,968 tokens): not shown preventable by the proxy
  configuration.** Prefix replay, stable tool order, tool-roster pinning,
  tail breakpoints, and one-hour cache TTL were already enabled. Their
  structural witnesses are stable. The non-monotonic older-snapshot reads
  point toward provider cache routing, replication lag, or eviction; current
  client-side logs cannot distinguish those. The largest event meets the
  prior five-figure re-open rule. Investigate with provider-side routing or
  cache-worker telemetry if available; an arbitrary delay/retry risks adding
  latency while not helping the two non-suspect reads.
- **CCR aftershocks (8,814 tokens): plausibly preventable with a tradeoff.**
  The proxy deliberately moves the newest cache marker onto hidden
  continuation messages. A CCR-only experiment that keeps the marker at the
  client-visible boundary could avoid charging hidden messages to the next
  client turn, while reducing cache reuse within the hidden retrieval rounds.
  See `ccr-continuation-cache-boundary.md` for a reproducible fixture and
  A/B measurements.
- **Early-message divergence (210,937 tokens): not safely preventable by
  replaying old content.** The same-request `prefix_replay_not_replayed` event
  says `first_diff_index=68`, shapes `tool_result,text` →
  `tool_result,text,text`, with only `plain` text kinds. It does not match the
  known `<system-reminder>` normalizer case. A `prefix_replay_applied` event
  also fired with `replayed_prefix=false`; that event can mean cache-control
  normalization and does not mean stale messages were replayed. The attribution
  is `origin=client` / `scope=stored_prefix`, but `chain_id=0` and conversation
  keys can merge streams, so the originating client operation is not proven.
  Do not restore the old content to save cache. The safe follow-up is to
  reproduce the metadata-only shape and establish stream identity before
  proposing a narrowly scoped canonicalizer. Details are recorded in
  `rejected/client-withdrawals-wont-fix.md`.
- **System-head changes (1,173 tokens): not a proxy-only cache bug.** Both
  turns have `head_moved=system` and changed beta. The client-facing system
  content changed too; retaining the old head would send different semantics.
  No fix is supported by this evidence.

The process command line observed around 13:25 local already enabled
`--prefix-replay true`, `--cache-tail-breakpoints 2`,
`--cache-stable-tool-order true`, `--cache-pin-tool-roster true`, and
`--force-1h-cache-ttl true`. The residual is therefore not explained by any of
those defenses being off.

### Reproduce this log audit without printing prompt content

The following Python 3 snippet rebuilds the fixed local-day window and prints
only timestamp, reason, provider landing, usage counts, and cache-boundary
arithmetic. The UTC cutoff freezes the same 15-event snapshot; remove the
cutoff comparison to inspect later additions. It never prints message
contents, request IDs, conversation keys, or session hashes.

```python
import json
from collections import Counter
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo

log = Path.home() / "headroom-proxy.log"
local_tz = ZoneInfo("Asia/Yerevan")
local_day = "2026-09-29"
cutoff = datetime.fromisoformat("2026-09-29T09:51:00+00:00")
events = []
starts = []

with log.open() as stream:
    for line in stream:
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        timestamp = record.get("timestamp")
        if not timestamp:
            continue
        when_utc = datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
        fields = record.get("fields", {})
        when_local = when_utc.astimezone(local_tz)
        if (
            fields.get("message") == "headroom-proxy starting"
            and when_local.date().isoformat() == local_day
            and when_utc <= cutoff
        ):
            starts.append((when_utc, fields.get("pid")))
        if (
            fields.get("event") == "cache_recache_observed"
            and when_local.date().isoformat() == local_day
            and when_utc <= cutoff
        ):
            events.append((when_local, fields))

print("starts:", [(t.isoformat(), pid) for t, pid in starts])
print("count:", len(events))
print("waste by reason:")
for reason, count in sorted(Counter(f.get("attribution_reason", "") for _, f in events).items()):
    total = sum(
        f.get("wasted_tokens", 0)
        for _, f in events
        if f.get("attribution_reason", "") == reason
    )
    print(f"  {reason}: {count} events, {total:,} tokens")
print("events:")
for when, f in events:
    actual = f.get("actual_cache_read")
    older = f.get("previous_previous_boundary")
    offset = (
        actual - older
        if isinstance(actual, int) and isinstance(older, int) and older >= 0
        else None
    )
    print(
        when.strftime("%H:%M:%S"),
        f.get("attribution_reason"),
        f.get("landing", ""),
        f"waste={f.get('wasted_tokens', 0)}",
        f"read={actual}/{f.get('expected_cache_read')}",
        f"prev={f.get('previous_cache_read')}",
        f"older={older}",
        f"actual_minus_older={offset}",
        f"race_suspect={f.get('commit_race_suspect')}",
    )
```

Expected summary at that cutoff: `15` events and `271,892` tokens; reason
totals `9 / 50,968` residual, `3 / 8,814` continuation aftershock,
`1 / 210,937` content divergence, and `2 / 1,173` head change. For the
13:07:16 threshold event, the arithmetic is `193,784 - 192,808 = 976` tokens
above the older boundary, versus the immediately previous read of `386,592`.

### CCR reproduction and experiment

Source path: `proxy/ccr_response.rs::build_ccr_continuation` appends the
proxy-private assistant/tool-result messages and calls
`proxy/continuation.rs::retail_continuation_breakpoint`. With the current
Anthropic config, that helper relocates the newest marker to the continuation
tail. The attribution comment in
`cache_stabilization/usage_observer_attribution.rs::observe_recache_cost`
explicitly describes the resulting aftershock: hidden retrieval rounds commit
a prefix the next client turn cannot match. The existing
`hidden_ccr_continuation_does_not_become_next_client_cache_baseline` test
checks the accounting baseline only; it does not check whether the provider's
cached boundary remains client-visible.

To reproduce the mechanism in a regression fixture:

1. Build an Anthropic request with two tail breakpoints and one tool call,
   then call `build_ccr_continuation` with an assistant continuation and tool
   result. Assert the current policy moves the newest marker past the
   proxy-private continuation, and that the request still has at most four
   markers.
2. Construct the next client request from the original client-visible
   messages, excluding those appended internal messages. Feed its usage and
   the prior client baseline through the usage observer. The live example at
   13:24:36 has actual/expected reads 120,994 / 125,107 and 4,113 wasted
   tokens; the next aftershock is 3,127 tokens.
3. In an experimental CCR-only branch, skip marker relocation for hidden
   continuation calls. Repeat with the same upstream usage fixture. Assert
   the marker remains at the client-visible boundary and the following turn
   no longer classifies as `aftershock_of_continuation`. Do not alter the
   user-visible body or exceed the provider marker cap.
4. Compare current and experimental policy on real CCR turns: subsequent
   client `cache_read_input_tokens` and aftershock waste, hidden-round cache
   reads/writes, and total cache cost per successful retrieval. A result is
   useful only if the reduction in next-turn recache waste exceeds any lost
   cache reuse inside CCR.

The expected tradeoff is lower hidden-round reuse if the marker stays before
the appended messages. Treat this as a measurement, not a default change.


## Detail

*moved from `docs/notes/recache-classification.md`*

## `unexplained_after_replay` — original-window closure, reopened by 2026-09-29

536 events, 527,163 tokens in the original 2026-08..09-02 window. The largest
bucket nobody had opened, and it is not a re-cache at all in that window.

Every event carries `matched_stream_msgs == turn_msgs == prefix_stable_msgs`:
the stored prefix matched the turn end to end, the replay went out, and the
provider read back a little less than the ledger expected. The distribution says
the same thing twice:

```
median   690        p90 2,034        p99 6,223        max 9,161
   0-  200:  12 events      2,014 tokens   0.4%
 200- 1000: 344 events    170,793 tokens  32.4%
1000- 5000: 173 events    306,104 tokens  58.1%
5000-20000:   7 events     48,252 tokens   9.2%
    20000+:   0 events
```

No tail. `prefix_content_diverged` put 1,998,513 tokens into 33 turns; the worst
single turn here is 9,161, and the total is a flat ~700 spread over 536 turns.
That is a breakpoint landing a block short of the divergence, or a 5m block
ageing out under a 1h one — the granularity of the provider's own accounting,
not a prefix we broke.

The original window was closed. Re-open if a later single event reaches five
figures; that would mean something real had started hiding behind the name.
The 2026-09-29 event above reaches that bar.

**Update 2026-09-02.** The name is gone. A second pass over 563 events (547K
tokens, 09-01..09-02) found the forwarded prefix byte-stable in every one; the
only thing that varied was where the provider's read stopped against the two
previous turns' boundaries. The observer now names that position instead, and
logs the four numbers it read (`actual_cache_read`, `previous_cache_read`,
`previous_boundary`, `previous_previous_boundary`) so each call can be checked
by hand. The reasons are `provider_missed_newest_write` (read equals the
previous read, the newest write was not found), `provider_partial_of_previous_write`
(read stops inside the previous write), `provider_free_read_not_persisted` (read
falls back to the older boundary after the previous turn read past anything
written), `provider_dropped_older_entry` (read is below the older boundary) and
`provider_between_entries` (the rest). `event_kind` stays `unexplained` and
`origin` stays `unknown`. All five are provider-side; none is ours.


## Audit framing

*moved from `docs/notes/recache-classification.md`*

## 2026-09-02 — live audit against the 09-01 binary

The window is `~/headroom-proxy.log` from 07:55:30Z on 09-01 to 12:22Z on
09-02, one binary, subscription auth, 9,481 booked turns. The log is live, so
every count here is a total at a moment, not a fixed one: a pass four hours
earlier in the same window read 8,706 turns and 503,030 tokens of recache
waste, and that figure no longer reproduces from any prefix of the file. Quote
the window with the number or the number means nothing.

```
booked turns                     9,481
cache read               1,106,564,274
cache write                 19,260,100
hit ratio                            98.3%
recache waste                  691,637   (3.6% of writes)
```

`scripts/proxy_log_audit.py recache` now prints that total first, then a table
by `attribution_reason`. It used to headline only the events carrying
`drift_dims` — 3 events and 192,549 tokens on this window — which is a
seventieth of the events and under a third of the tokens. The reasons:

```
reason                          events      tokens   median
unexplained_after_replay           339     308,083      615
tools                                2     137,456   75,145
early_messages                       2     136,208   74,830
system                               1      55,093   55,093
concurrent_turn_in_flight          101      43,659      240
prefix_content_diverged             17       6,699      239
aftershock_of_diverged_prefix        5       4,439      238
inbound_tail_replaced                2           0        0
```


## Tail confirmation

*moved from `docs/notes/recache-classification.md`*

- **`unexplained_after_replay` is gone by rename, not by elimination.**
  The 09-02 rename into five `provider_*` reasons holds; this window:
  `provider_dropped_older_entry` 117 ev / 3.58M tokens,
  `provider_free_read_not_persisted` 61 / 1.76M, `provider_between_entries`
  47 / 109k, `provider_missed_newest_write` 12 / 5k,
  `provider_partial_of_previous_write` 1. Total 238 events / 5.45M — now the
  largest waste class by tokens. The provider-side attribution stands
  unchallenged (no new evidence either way); the 09-02 sidecar subset cannot
  be re-tested by name anymore. If this class is ever worked, it starts here,
  not at the 09-02 sidecar numbers.
