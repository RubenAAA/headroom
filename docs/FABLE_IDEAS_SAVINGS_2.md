# Cache writes: what is workload, what is loss, what to fix

Measured on the live log (`~/headroom-proxy.log`, 2026-09-03 14:50Z–17:45Z)
and the rotations `.log.1` (09-03 08:01Z–13:22Z), `.log.3` (09-01/02) and
`.log.4` (08-31). Event `turn_cost_ledger` gives per-turn
`cache_read_input_tokens` / `cache_creation_input_tokens` / `input_tokens`
by `conversation_key`; `prefix_composition`, `prefix_replay_not_replayed`,
`cache_recache_observed`, `messages_rewritten`, `ctx_offload_accounting`,
`prior_thinking_dropped`, `sidecar_*` are joined by `request_id`. Scripts:
`/tmp/wr_analysis.py`, `/tmp/floor.py`, `/tmp/chase.py`..`chase6.py`,
`/tmp/stall.py`, `/tmp/evrate.py`.

## 1. The theory that failed

The aggregate write:read ratio went from 0.013–0.015 (08-31, 09-01/02) to
0.035–0.040 today. The first guess was fan-out: many short subagent
sessions, each paying a big first-turn write against little read.

Not so. Writes by conversation lifetime:

| lifetime (turns) | 08-31 | 09-01/02 | 09-03 AM | 09-03 PM |
|------------------|------:|---------:|---------:|---------:|
| 1                |  2.9% |     4.8% |     3.1% |     5.4% |
| 2–5              |  4.5% |     7.6% |     1.9% |     0.1% |
| 6–20             |  7.5% |    11.5% |    11.6% |    12.1% |
| 21+              | 85.1% |    76.1% |    83.4% |    82.4% |
| 21+ bucket w:r   | 0.013 |    0.015 |    0.036 |    0.031 |

One-turn sessions hold about 5% of writes on every day. The ratio doubled
inside long conversations.

Siblings that share a system+tools prefix do read it from cache: nine
conversations read exactly 23,682 tokens (opus, tools fingerprint
`f96a35acddee`, 36 kB tools + 5 kB system). Their 42–55k writes are each
session's own task prompt (`input_tokens` 1–2 on those turns). Re-paid
prefix across all shared fingerprints today: about 15k tokens, 0.3% of
writes. Five of those nine 23,682 reads were not first turns at all; see
section 4.

## 2. What the doubling is

Per consecutive turn in a session, `growth = (read + creation + input)_now −
(read + creation + input)_prev` is how many new prompt tokens entered the
conversation. A perfect cache writes exactly `growth`. `excess =
max(0, write − growth)` is content re-written that was already cached.
Turns with `growth < 0` are set aside for section 3.

| day      | turns | growth p50 | write p50 | floor | excess | excess tokens |
|----------|------:|-----------:|----------:|------:|-------:|--------------:|
| 08-31    |  6124 |        802 |       789 | 85.0% |  15.0% |         1.19M |
| 09-01    |  3511 |        862 |       847 | 92.6% |   7.4% |         0.35M |
| 09-02    | 12061 |        814 |       794 | 86.2% |  13.8% |         2.24M |
| 09-03 AM |  2346 |       1808 |      1998 | 82.0% |  18.0% |         1.26M |
| 09-03 PM |  1545 |       2044 |      2065 | 85.6% |  14.4% |         0.77M |

Growth per turn doubled and write per turn doubled with it; the floor share
is flat. Assistant output moved little (median `output_tokens` 307–315
before, 315–399 today), so the growth is on the input side: tool results.
Today's work was log crunching with large tool output. That is a workload
shift, and the proxy is not re-writing cached content on ordinary turns any
worse than before.

Excess by cause, share of that day's excess (flags overlap):

| flag                | 09-03 AM        | 09-03 PM       |
|---------------------|-----------------|----------------|
| system fp changed   | 464k (37%, n=73)| 104k (14%, 41) |
| sidecar adjacent    | 462k (37%, 267) |  62k (8%, 42)  |
| thinking dropped    | 357k (28%, 62)  |  27k (3%, 14)  |
| rebuild boundary    | 254k (20%, 48)  |  47k (6%, 4)   |
| tail diverged       | 116k (9%, 37)   |  36k (5%, 14)  |
| msg1 collapse       |  76k (6%, 2)    |  —             |
| tools fp changed    |  63k (5%, 17)   |  36k (5%, 5)   |
| flagged / unflagged | 76% / 24%       | 26% / 74%      |

On baseline days the flags cover 1% (08-31), 19% (09-01) and 46% (09-02,
mostly `tail_diverged` 620k and one tools change 310k).

## 3. The tail the floor hides

`growth < 0` means the cached total shrank: read fell further than write
rose. That is exactly what a collapse looks like (read drops to the shared
23,682 block, the rest is re-written), so these turns carry the losses.
Their writes as a share of that day's later-turn writes:

| day      | turns | writes | share | attributable inside |
|----------|------:|-------:|------:|---------------------|
| 08-31    |   342 |  581k  |  6.8% | — |
| 09-01    |   189 |  268k  |  5.4% | — |
| 09-02    |   793 | 2.82M  | 14.8% | — |
| 09-03 AM |   254 | 1.06M  | 13.1% | thinking dropped 260k, msg1 collapse 198k, sidecar 75k |
| 09-03 PM |   161 | 1.63M  | 23.3% | tracker lost 410k, msg1 collapse 270k, thinking dropped 146k, system-prompt change 213k, sidecar 17k |

About half of today's tail is client or restart origin. The rest is
proxy-side and fixable: roughly 400k AM and 165k PM.

## 4. The causes, one by one

### 4.1 Client inserts a `role:"system"` message at `messages[1]`

`messages_rewritten.early_fingerprints` shows the shift on the collapse
turn: old message 1 becomes message 2, message 0 gets a new hash, and the
new message 1 has the same hash `cb27b5f9` in every session.
`prefix_replay_not_replayed` reports `first_diff_index=1`,
`first_diff_path=role`, stored head `assistant`, current head `system`,
`current_original_msgs = stored + 3`.

That hash at index 1 appears 0 times on 08-31 and 09-01/02, 691 times in
`.log.1` (first at 08:33:24Z) and 76 times live. Cost when it lands
mid-session: read collapses to 23,682 and the conversation is rewritten.
Live: 6 turns, 222,716 tokens. AM: 5 turns, 274,506. Client origin; the
proxy declines replay correctly (`prefix_replay.rs:304` already knows the
API accepts system-role messages). `cache_recache_observed` tags these
`origin=proxy / early_messages / forwarded_hot_zone`, which is wrong.

What to do: nothing on the cache path. Fix the attribution. If the message
is a fixed directive, a replay chain could splice around it; that is a
design question, not a bug.

### 4.2 `history_rewritten` + `prior_thinking_dropped` mid-conversation

`proxy.rs:3462` defines `history_rewritten` as "the replay store cannot
replay this session's history"; `proxy.rs:3469` makes that an offload
boundary, and `compression/prior_thinking.rs` runs on it on the premise that
the provider writes the prefix fresh anyway.

`ctx_offload_accounting` with `history_rewritten=true`: 0 of 16,472 on
09-01/02, 49 in AM, 95 live. Later turns carrying `prior_thinking_dropped`:
AM n=105, 84.8% with a read shortfall, 800k re-write; live n=21, 85.7%,
625k. Inside the `growth < 0` tail it is 260k AM and 146k PM.

The premise holds when the client really diverged at the head (4.1). It
also fires on tail-only divergences (`first_diff_index` 37..310, 11–22
turns per file, ~30k re-write each) where only the tail would have been
rewritten; there the drop rewrites every earlier assistant message too.
Conversation `40496550` lost 1.5–3k on every turn this way
(`prefix_content_diverged` at 37, 48, 69, 79, 82, 92; seven drops).

What to do: run the drop only when the divergence index is 0 or 1, or when
the tracker is gone. A tail divergence is not a fresh write of the head.

### 4.3 Spinner-text sidecar falls back two times in three

`sidecar_detected` 557 in AM, 378 fell back; live 99 detected, 62 fell
back, every one with status 400 "The long context beta is not yet available
for this subscription" (`sidecar.rs:388` passes the client's beta header to
haiku). A fallback forwards the original request in full, under a different
system prompt. Ledger cost of fallback turns: AM 182 turns, 627,133 write
tokens (6.8% of the file), median read 48k; live 29 turns, 100,355.

The different system prompt is a drift: 17 of 50 `system` drift events and
16 of 30 `early_messages` drift events in AM are the sidecar request
itself, and each drift drops the replay store
(`prefix_replay_invalidated_on_rebuild` 43/1000 turns AM vs 1/1000
baseline; later turns carrying it: AM n=95, 92.6% shortfall, 590k re-write).

What to do: strip `context-1m` from the sidecar's beta header. Then the
answer is a haiku call on 2–4 messages and never touches the main session's
cache. In AM the fallback error body is logged as undecoded compressed
bytes; live logs it decoded.

### 4.4 System-prompt change after a stream failure

Request `5e27a02e` (conversation `b3d793f2`, 15:57:03Z) wrote 213,309
against 15,621 read. Sequence: `c81c64a0` (217 msgs, system fp
`2c01d76ae29a`, 16,125 B) got a 200 and then died mid-stream ("error
decoding response body"); the proxy synthesised a truncated tail. The client
resent 220 msgs with system fp `6f8ff11af482`, 10,145 B — 6 kB shorter —
as `ce3ff134` (two network retries, never completed) and again as
`5e27a02e` (one retry, 200). Read 15,621 is the unchanged 42.5 kB tools
block; everything after the system block was rewritten.

`cache_recache_observed` said `concurrent_turn_in_flight /
provider_cache_timing`, because `ce3ff134` was still retrying. The cause is
the system change; `prefix_composition` shows it. Client origin. Later
turns where the forwarded system fingerprint changed: 0.0% and 1.2% of
writes on baseline days, 10.3% AM, 11.4% live.

What to do: fix the attribution order — check `prefix_composition`
fingerprints before the in-flight explanation.

### 4.5 Tool roster flaps by one tool

`tool_roster_changed` 12 events live, all "removed SendUserFile 21→20";
`tools_before` alternates 23/24 or 27/28 within 20 of 55 live sessions and
36 of 75 AM. Each flip is a tools drift, a rebuild boundary and a dropped
replay store. Cost: live 4 turns 102,570 write; AM 11 turns 113,706. Client
origin, small, regular.

### 4.6 Restarts and idle

Two non-first turns live hit `no_tracker_for_session` after the 15:58:38Z
restart, 314,729 tokens; one of them (`f52eef6e`, 169,675) was also 62
minutes idle, past the 1h TTL, so the restart changed nothing there.

## 5. Not proxy faults, for completeness

- Upstream 529/overloaded retries 124 per 1000 turns live (2 baseline);
  "error inside a 200 stream" 88; `stream_incomplete` 21.
- Forwarded TTFB median 2.2 s live vs 1.3–1.4 s baseline once the morning
  stall ended (see `FABLE_IDEAS_SPEED.md`).
- CCR continuation (`sending continuation` → `retrieval handled`) median
  62 s at 16h live (n=12) vs 4–11 s baseline; 13 requests over 40 s. The
  log has nothing between the two lines; cause open.
- Outbound body inflation (p90 +76–109 kB today, negative before): checked
  and closed. Inflated turns carry `prefix_replay_applied` and have w:r
  0.024 against 0.044 for shrunk turns; the proxy re-expands a prefix the
  client trimmed so the cache still hits.

## 6. How to know it worked

Re-run `/tmp/floor.py` after a day of similar work. Targets:

- `growth < 0` turns' writes under 8% of later-turn writes (08-31/09-01
  level), from 23% PM today.
- `prior_thinking_dropped` on later turns with `first_diff_index > 1`: zero.
- `sidecar_fallback` with status 400 on the beta message: zero;
  `sidecar_detected` turns absent from the ledger.
- `cache_recache_observed` with `origin=proxy` on a turn whose
  `prefix_replay_not_replayed` has `first_diff_index=1` and path `role`:
  zero.
- Excess share stays within 7–15%; if it rises with the tail fixed, that is
  a new problem.
