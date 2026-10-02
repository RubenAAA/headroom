# Idea: Jev-guided context refresh in place of client compaction

- **Status:** observe mode built 2026-10-01 (`ctx/refresh.rs`,
  `--ctx-refresh-observe-tokens`): it scores and logs, and changes no request.
  The apply half is unbuilt and needs a lot of tuning before it can run on a
  live session (see "Tuning" and "Findings"). Big contexts do hurt: a Spark
  session that passed the 1,048,576-token window got a generic 400 on every
  turn after (provider-reported 1,048,803 on the last good one).
- **Source:** request 2026-10-01 (Spark sessions run to "crazy" context);
  `docs/jev-model.md` for what Jev can do and its limits;
  `spark-usage-and-compaction.md` for the context sizes.
- **Idea:** when a session passes a token trigger, Jev judges which parts of
  the history the current task still needs. The proxy keeps those verbatim,
  swaps the rest for stubs the model can fetch back (`headroom_retrieve`), and
  forwards the shorter history. That is what `/clear` plus a hand-picked restore
  would do, with no compaction from the client. Aimed at Spark: on 2026-09-29,
  6,044 turns, `tok_after` p50 335,660, p90 627,419, max 844,515, sessions of
  2,800 messages. Claude Code never compacts them, because the proxy tells it
  `input_tokens: 0`.

## How it would work

- **The proxy cannot send `/clear`.** That is a client command, and the client's
  own transcript keeps growing. What the proxy can do is rewrite the `messages`
  it forwards, as `--ctx-offload-stale-messages` already does for old tool
  results.
- **Decide once, replay.** A rewrite that changes every turn breaks the cached
  prefix every turn. Follow `ctx/inject.rs`: at the trigger, build the shorter
  history once, store it keyed by conversation, and send that base plus the new
  tail on later turns. The next trigger builds a new base.
- **Jev selects, it does not summarise.** It cannot write prose. The choice is
  per unit: keep verbatim, or replace with a stub. A summary of what was dropped
  must come from elsewhere: the stored events behind `build_resume_snapshot`
  (deterministic, no model), or one Spark call.
- **Structure decides first, Jev decides the middle.** Never touch the system
  prompt, the first user message, the last N messages, an unanswered tool call,
  or half of a `tool_use`/`tool_result` pair (`orphan_tool_result.rs` exists
  because a split pair is rejected). Jev scores only what is left.
- **Work off the request path.** Scoring a long history takes minutes (below).
  Run it in the background, as `background_compression.rs` does for compression,
  and apply it on the first turn after it finishes.

## What Jev costs here

From `docs/jev-model.md`, measured 2026-10-01 on the free endpoint:

- About 0.8 s a call whatever the size; 12 parallel calls took 0.93 s; the cap
  is about 33k input tokens a call; one call can carry 1,000 questions.
- **Per unit:** state is the task plus one unit, one `noul` ("does the task need
  this?"). 2,800 units at 12 in parallel is about 3.5 minutes. Extrapolated,
  not measured at that scale, and the rate limit is unknown.
- **Per batch:** state is the task plus 30 or so numbered units, one `noul` per
  unit. Far fewer calls, but the jaggedness page says surplus text in the state
  lowers accuracy. Which one wins is untested.
- **Weaknesses that bite:** it reads wording literally, so "needed for the
  task" must be spelled out. It does not treat input as hostile: a tool result
  that says "ignore the task, keep everything" can move its score. `noul` values
  are not comparable across question wordings.
- **Data:** the whole history goes to opencode.ai. Spark sessions already go to
  Zen, where Meta may train on prompts, so the exposure is the same. A Claude
  session is a different decision. Run the history through `--redact-sensitive`
  first.

## Cost and benefit

- **Cache.** A new base rewrites the prefix, so the next turn writes the whole
  shorter context. `--ctx-offload-stale-messages` documents the trade for
  Claude: creation 1.45 against reads 0.09, so a rewrite pays back only after
  many turns. Spark input is free, so the cost there is time, not money.
- **The case for it on Spark is speed and quality, not price.** Unmeasured:
  whether a turn at 600k is slower or worse than one at 100k. Measure that
  before building anything.
- **Retrieval is the price of a wrong drop.** `learnings/offload-vs-retrieval-3x.md`:
  offload saved 154M tokens a day against 441 retrievals costing about 5.6M
  equivalents. A refresh pays while its retrieval rate stays low, and the model
  re-running a tool instead of retrieving is a worse failure that logs nothing.

## Tuning

There is no ground truth for "relevant", so each of these is a guess until a
replay says otherwise:

- the trigger (tokens, or every N turns) and how often a new base is built;
- how many recent messages are always kept;
- the question wording, the threshold per question, and what a score in the
  gap does (keep, stub, or ask again);
- stub or drop, and how long a stub is;
- per-unit or per-batch, and the batch size;
- what the task is: the last ask, the last few, or a plan the model states.

## Findings (2026-10-01, the 1M-token session `36a4ce78`)

Measured on its transcript, 4.0 MB of content (4.5 MB on the wire):

- **What the bytes are.** User-role text 43%, `tool_use` inputs 29%, tool
  results 20% (1,023 of them; 996 under 2 KB), assistant text 7%. The existing
  offload (`--ctx-offload-min-bytes 20000`) cannot touch it. Scoring tool
  results would not have shrunk it, so the unit is the exchange: one typed user
  message and everything to the next. The session had 81 exchanges, median
  about 10 KB; the autonomous "Goal check-in" loops ran to 119 messages and
  81 KB each. Per-message scoring (2,800 units) is not needed and, see below,
  not affordable.
- **Scores bunch.** One `noul` ("does the current task need the details of this
  earlier exchange?") on 70 exchanges: quantiles 0.14, 0.42, 0.47, 0.64, 0.72,
  0.78, max 0.92. Only trivia (`[Request interrupted]`, `/model` output) scored
  low; the large loops scored about 0.8. A threshold keeps nearly everything
  that is big, which is the opposite of what a refresh needs. The control test
  (an exchange against its own task, an unrelated task, another exchange's
  task) was blocked by the rate limit at first; done later through the egress
  lanes on a synthetic 10-exchange request: related exchanges scored 0.50-0.62,
  unrelated 0.29-0.48. The ranking is right but the scores are compressed, so a
  fixed threshold is fragile; use a percentile or a gap to the task's own score.
- **The free endpoint rate-limits, per exit IP.** 429 after about 140 calls in
  a few minutes from the machine's address, while all 9 egress lanes still
  answered 200. Observe mode and `headroom classify` now go through the lanes;
  see `docs/jev-model.md` Limits.
- **Window overflow looks like a generic 400.** Provider-reported 1,048,803
  input tokens on the last good turn; every turn after got "The request
  contains invalid parameters". The proxy's own estimate ran about 6% under the
  provider's (980k against 1.05M), so a trigger set from the estimate needs
  that margin.

## Observe mode (built)

`ctx/refresh.rs`, called from `routed/transforms.rs`. Off unless
`--ctx-refresh-observe-tokens N` is set; `contrib/headroom-flags.sh` sets it to
400000. For a Spark-named model past N estimated tokens (4 bytes a token), once
per session per 30 minutes, a background task:

1. splits `messages` into exchanges and takes the last typed request of 20
   characters or more as the task;
2. scores up to 100 exchanges (largest first; never the opening one, the newest
   five or the task's own), 3 at a time, one `noul` each, with a pause on 429;
3. logs one `ctx_refresh_exchange` per scored exchange (index, messages,
   tokens, score, first 100 characters of the request) and one
   `ctx_refresh_observed` with the exchanges and tokens that would be stubbed
   under scores of 0.3, 0.4 and 0.5.

The exchange text goes to opencode.ai after `--redact-sensitive`, as the Spark
turn itself does. Nothing in the request changes. Read it with:

```bash
grep ctx_refresh_observed ~/headroom-proxy.log | tail
grep ctx_refresh_exchange ~/headroom-proxy.log | tail -100
```

What to look for before building apply: whether the low-scored exchanges are
ones you agree are dead, whether the tokens under 0.3 or 0.4 are worth a cache
rewrite, and how often `errors` is not zero.

## Pipeline (agreed 2026-10-01)

1. **Prune without a model.** Dedup, offload and compress what old history
   holds, using what `transforms/` already has. Nothing here needs Jev.
2. **Jev pass.** Score what is left against the last few typed turns; stub the
   exchanges below a threshold (percentile or gap, not a fixed number; see
   Findings).
3. **Keep the last N turns verbatim**, the opening message, any unanswered tool
   call, and thinking blocks in the recent window.
4. Spark input is free, so a rewrite costs time, not money. Scoring still takes
   minutes and each exit IP has a Jev quota, so build the base in the
   background at a token trigger and persist it, not per turn.

## Stage 1 measurement (2026-10-01, session `36a4ce78`, 3,571 messages)

Harness: `crates/headroom-core/examples/refresh_measure.rs` (scratch,
uncommitted). Content about 4.0 MB; everything but the newest 20 messages
eligible.

| Pass | Result |
|---|---|
| `cross_turn_dedup` on tool results (as the proxy runs it) | 768 KB to 753 KB (-2%) |
| same dedup on user-role text (not wired today) | 1.74 MB to 1.59 MB (-9%) |
| `text_crusher` (lossy, extractive, ratio 0.5) user text | 1.74 MB to 0.96 MB |
| `text_crusher` `Agent` prompts | 792 KB to 448 KB |
| `text_crusher` tool results | 763 KB to 477 KB |
| `text_crusher` assistant text | 280 KB to 216 KB |

- **Dedup is small here.** A line-level count said 53% of tool-result bytes
  repeat, but the pass folds only runs of 3 or more contiguous repeated lines,
  and few are. Lossless, but about 4% of the session at best.
- **Together** dedup and `text_crusher` take about 4.0 MB to about 2.5 MB
  (roughly 1M to 620k tokens). Rough: the passes overlap, the crusher ran with
  a placeholder task, and output quality is unchecked.
- **User text is 43% and is mostly not typed.** `<task-notification>` subagent
  reports and 107 near-identical "Stop hook feedback: Doc drift" messages.
  Dedup on user text is the cheap lever; the reports are prose a crusher or a
  Jev stub can take.
- **Not measured:** Kompress (needs ONNX), `smart_crusher` and the log, search
  and diff compressors (tool results are only 20% of the session).
- **`E1`-`E4` in this repo are cache-stabilization PRs** (tool order,
  `cache_control`, `prompt_cache_key`), not compressors.

### User-text dedup (built 2026-10-01)

`--cross-turn-dedup-user-text-tail N` (shipped on at 8 in
`contrib/headroom-flags.sh`), with `--enable-cross-turn-dedup`. On the routed
path (Spark, Codex) `cross_turn_dedup` now also folds spans of 3 or more
repeated lines in user-role text, as it already did in tool output. The newest
N messages are never rewritten, because the live request must reach the model
verbatim. `dedup_messages_with_user_text` in the core; `dedup_messages` is
unchanged, and so is the Anthropic path.

- Real function on the 1M session, tail 8: 442 spans, 164 KB removed, user text
  1.74 MB to 1.59 MB (-9%); total body 6.59 MB to 6.43 MB (-2.5%).
- **Cost:** a message ageing out of the tail is rewritten on a later request,
  so the provider's cached prefix changes once per message. Free for Spark;
  keep it off for any model where the cache is paid.
- Test: `user_text_dedup_folds_old_repeats_on_a_routed_model_and_keeps_the_tail`
  (flag off: three copies forwarded; on: the middle one a pointer, the newest
  whole). Core tests: tail 0 changes nothing, pointer at the first copy,
  prefix-monotonic.
- Still the small lever: the bulk of user text is distinct subagent reports, a
  job for a crusher or a Jev stub.

### Stage 1 replay, in order (2026-10-01, same session)

`refresh_measure` `stage1()`: everything but the newest 20 messages; tokens are
text and tool bytes at 4 a token plus blob characters at 8.5 a token (measured
7.5 to 10), so read the totals as about, not exact (the provider counted
1,048,803 at the last good turn; this model gives 1,137k at the start).

| Step | text B | tool B | blob chars | about tokens |
|---|---|---|---|---|
| start | 2,043,945 | 1,919,486 | 1,243,074 | 1,137k |
| drop old thinking | 2,023,192 | 1,919,486 | 0 | 986k |
| dedup, user text included | 1,873,957 | 1,904,978 | 0 | 945k |
| `text_crusher` on old prose | 1,070,483 | 1,453,252 | 0 | 631k |

- **Blobs are the largest step (-151k), dedup the smallest (-41k).** The
  crusher is the next largest (-314k) and the only one that loses information.
- **The crusher is not safe on subagent reports as is.** On the first
  `<task-notification>` it kept the table rows and dropped the opening tag, the
  `<summary>` and the head of the `<result>`: the "Recon report" title and
  `BASE sha` line were gone. Extractive scoring by recency, a placeholder-ish
  relevance and salience does not know which lines are identifiers. One sample,
  not a verdict, but it argues for Jev (or a keep-identifiers rule) deciding
  what is dropped from reports, and for keeping the first lines of a block.
- Without the crusher, blobs plus dedup reach about 945k: under the window but
  not by much. With it, 631k. The gap between them is what Jev's stub decision
  has to cover better than the crusher does.

### Thinking

- Visible thinking text: 215 blocks, 20 KB (0.5%). The `signature` field,
  which carries the encrypted reasoning blob inside a Headroom envelope, is
  1.24M characters, bigger than any readable category. Nothing can read it, so
  "relevant info in old thinking" can only live in the 20 KB.
- `--zen-reasoning-replay true` sends the blobs upstream. Zen refuses a blob
  issued to another caller (`routed/reasoning_blobs.rs`), which the proxy
  already handles by dropping it.
- **Blobs count toward the window (measured 2026-10-01).** Through the proxy,
  one streamed Spark turn with a thinking block, then the same history replayed
  with and without the thinking blocks, provider-reported `input_tokens` from
  `/spark-context?session=`:

  | Run | envelope chars | visible thinking text | with blob | stripped | diff |
  |---|---|---|---|---|---|
  | 1 | 11,086 | n/a | 4,594 | 2,968 | 1,626 |
  | 2 | 22,009 | 323 chars | 5,287 | 3,025 | 2,262 |

  About 7.5 to 10 envelope characters per token (the visible text is under 100
  tokens). The 1M session's 1.24M envelope characters are therefore about
  125k to 165k tokens, 12 to 16% of the window and several times what dedup
  frees. They also explain why the proxy's own estimate ran under the
  provider's. Dropping old blobs is the biggest single stage 1 lever.
  Unmeasured: whether dropping them hurts later turns.
  (Earlier attempts failed: non-streamed and short prompts return no thinking
  block, and a direct call to Zen is refused outside OpenCode, so measure
  through the proxy with `stream: true` and a reasoning prompt. A turn takes
  about 40 s.)
- **Built:** `--zen-reasoning-keep-recent N` (`routed/reasoning_blobs.rs`
  `drop_old`, called in `routed/translation.rs` after `drop_refused`; set to 10
  in `contrib/headroom-flags.sh`). On a Zen turn only the newest N blobs are
  replayed; older items lose `id` and `encrypted_content` and keep their
  summary, the same shape `drop_refused` already sends. `0` keeps all. A blob
  ageing out changes the forwarded prefix once. Unit test only
  (`old_blobs_are_dropped_and_the_newest_n_replayed`): the mock upstream is not
  a Zen host and the switch is process-wide, so an end-to-end test needs a
  Zen-shaped route. Not yet checked live: after a restart, `/spark-context`
  `input_tokens` on a long session should fall by about 600 tokens per blob
  dropped, and `zen_reasoning_blobs_dropped` events should not rise.
- Not decided: whether to drop visible thinking text too (20 KB, negligible).

### `Bash` and `SendMessage` inputs

- `Bash`: 360 commands, 155 KB of command text (186 KB as JSON). Median 400
  characters, largest 1.6 KB, none over 2 KB. Mostly inline `python3 -c`
  one-liners and `git --git-dir=.worktrees/...`. Crushing the 77 over 600
  characters would save about 28 KB (under 1% of the session) and could damage
  a command the model may copy.
- `SendMessage`: 198 calls, 70 KB of message text, median 269 characters. Short
  nudges to subagents; 24 over 600 characters would save about 10 KB. The
  `content` field is a truncated copy the harness adds, not a second full copy.
- **Decision:** leave both to the exchange level. They are too small and too
  varied to pay for a per-block pass, and a stub of the whole exchange removes
  them anyway. `Agent` prompts (846 KB, 408 calls) are the large `tool_use`
  input, and `text_crusher` takes them to 57%.

## Jev pass over the stage 1 output (2026-10-01, same session)

Offline, `/tmp/jev/jevpass.py` and `jevpass3.py` (scratch): the stage 1 output
(old thinking dropped, dedup with user text; no crusher), split into 191
exchanges, 185 scored (not the opening, the newest 5, or the task's own), one
`noul` each through the egress lanes, 0 errors. Same question as observe mode.
Tokens below are normalised to the stage 1 total (about 950k).

- **Scores bunch and ignore size.** Task = last typed request: quantiles 0.16,
  0.33, 0.40, 0.46, 0.53, 0.58, 0.75 (min, p10, p25, median, p75, p90, max).
  Task = last three typed requests: 0.15, 0.37, 0.49, 0.54, 0.61, 0.67, 0.78.
  The ranking is stable (Spearman 0.89 between the two; 34 of the 46 lowest
  agree), but the mid-range moves with the task text, so a fixed threshold does
  not transfer. Percentiles do.
- **The big exchanges score mid to high.** The ten largest (25k to 69k each,
  80 to 245 messages) are "Goal check-in" loops of one orchestrator prompt and a
  `/compact` summary: 0.48 to 0.72. Scores under 0.3 are the trivia (repeated
  hook feedback, a stray question): 14 exchanges, 10k tokens.
- **Stubbing whole exchanges on score frees little.** Lowest 25% by count:
  about 50k to 75k. Lowest 50%: about 140k to 180k. Under 0.5 with the
  one-request task: 307k, but 121 of 185 exchanges, including large ones at
  0.40 to 0.48 that nobody has checked.
- **The task is weak in an orchestrator session.** The last typed requests are
  "make sure to use all 8 spark agents", not the audit the loops are about.
  Jev scores relevance to what was typed last, not to what the session is for.
- **Payload offload beats it.** Stage 1 is 951k, of which tool inputs and
  results are 487k and text 464k. Replacing every tool payload over 400 bytes
  in messages older than the newest 20 with a 150-byte retrievable stub
  (the existing offload, lowered thresholds) saves about 392k alone, to 559k,
  and loses nothing the model cannot fetch back. Adding Jev stubs for the lowest
  25% of exchanges takes it to 536k, the lowest 50% to 481k: Jev's share is 23k
  to 78k on top. Estimates from byte counts; no model was run on the result.
- **Read:** the order is stage 1, then tool-payload offload with Spark-specific
  thresholds, then a Jev stub for the lowest exchanges as a last trim. Jev is
  the smallest of the three.

### The real offload on the stage 1 output (2026-10-01)

`crates/headroom-proxy/examples/offload_measure.rs` (scratch): the shipped
`offload_anthropic_request` (no gate) plus `offload_tool_use_inputs` (rebuild
boundary) on the stage 1 messages older than the newest 20 (3,551 messages,
5.12 MB of JSON), at several `min_bytes`:

| `min_bytes` | tool results offloaded | tool_use strings offloaded | JSON saved |
|---|---|---|---|
| 400 | 528 (282 KB) | 488 | 831 KB (16.2%) |
| 1,000 | 470 (272 KB) | 418 | 813 KB (15.9%) |
| 2,000 | 25 | 164 | 400 KB (7.8%) |
| 4,000 | 12 | 5 | 88 KB (1.7%) |
| 20,000 (today's default) | 0 | 0 | 0 |

- **The byte-count estimate above was too high.** It assumed 150-byte stubs and
  392k tokens saved. The real stub is about 700 bytes (a 512-byte preview cap
  plus the footer: median 693, p90 713 after offload), and more than half the
  tool results are under 700 bytes already. Real saving at 400 to 1,000 bytes:
  about 16% of the body, roughly 200k tokens, not 392k.
- **Most of it comes from `tool_use` inputs, not results.** 549 KB of the 831 KB
  is large strings in old tool calls (the 846 KB of `Agent` prompts). Needs
  `--ctx-offload-tool-use`; it is off unless set. The tool_result pass alone
  gives 5.5%.
- **The 20,000-byte default frees nothing on this session**, as found earlier.
  The lever is the threshold: 400 to 1,000 for Spark.
- Not measured: how often the model would need `headroom_retrieve` for a
  stubbed block, or whether it re-runs the tool instead (`learnings/offload-vs-retrieval-3x.md`).
  No model was run on the result.
- **Built: `--ctx-offload-spark-min-bytes N`** (`routed/transforms.rs`
  `offload_tool_results`; `0` = off, the default; shipped commented out at 1000
  in `contrib/headroom-flags.sh`). For a Spark model it sets the offload
  threshold to N, adds the `tool_use` input pass, lets first conversions happen
  on any turn (Spark's cache is free), and detaches the newest 20 messages so
  neither pass touches them. It also turns offload on for Spark on the Zen
  route, which is otherwise skipped without `--ctx-offload-zen`. Needs
  `--ctx-offload` (the launcher sets it). Logs `ctx_offload_spark`. Test:
  `spark_offload_stubs_old_tool_results_and_keeps_the_newest_twenty_messages`
  (off: all 15 results and 15 tool_use inputs whole; on: the five old of each
  become digests, the newest 20 messages stay whole).
- **Why it is off.** On Claude, a 2,000-byte floor lost: hidden
  `headroom_retrieve` rounds and re-reads cost 126% of the saving
  (`learnings/offload-loses-at-2000-bytes.md`), and on Zen a retrieval round is
  2.2 s the client never sees (`--ctx-offload-zen` doc). Spark's tokens are
  free, so the loss is time and wrong answers, unmeasured here. Not checked
  live: that conversions survive `--prefix-replay` on a second turn, and the
  retrieval rate at 1000. Try it on one session and read `ctx_offload_spark`
  and the `headroom_retrieve` count.
- **Live finding (2026-10-01, 16:25 to 17:20): the `tool_use` pass makes the
  model forge digests.** With the flag on, the lead agent of a 10-agent Spark
  team saw its earlier `Agent` prompts as a 600-byte preview plus a
  `<<ctx:HASH>>` pointer, and began writing new prompts in that shape. Nine
  `Agent` prompts in a row, 16:31 to 17:19, are 671 to 678 characters, cut
  mid-sentence ("... (reuse e") and ending in a pointer; before 16:25 they were
  2.9 KB. The pointer hashes are invented: all 9 are absent from the store (3 are 23 or 25 characters
  long), and none was ever handed out in a digest. The subagents therefore received a truncated brief and then
  tried to retrieve a hash that does not exist. Digests the proxy did hand
  out (586 since the restart) were all in the store, so the offload and the
  store are sound; the harm is the model copying the digest format into its own
  output. The `tool_result` pass has not shown this.
  Fix options: drop the `tool_use` pass for Spark (keeps the 5.5% from results,
  loses two thirds of the saving); or exempt `Agent` and `SendMessage` inputs,
  which the model reuses as templates, though that is most of the tool_use
  bytes; or keep the digest out of text the model can copy.
- **Revised stack for this session**, in tokens (JSON bytes over 4): stage 1
  (old blobs dropped, user-text dedup) from about 1,137k to 945k; offload at
  1,000 bytes with tool_use on takes about another 200k, to about 740k; Jev
  stubs for the lowest 25 to 50% of exchanges, 23k to 78k more, to about 660k to
  720k. Under the window with room, but not the 500k a size-only guess gave.

## Overlap

- `spark-usage-and-compaction.md` takes the other road: report the true input
  tokens so Claude Code compacts by itself. That compaction sends 300k to 800k
  of context to Spark with a summary prompt and loses detail. This idea keeps
  verbatim text and stubs the rest. They are alternatives; do not build both
  before the first one is measured.
- `--ctx-offload-stale-*` already stubs old tool results by age. This replaces
  age with relevance.
- `rejected/per-turn-model-routing.md` is the warning: a per-turn rewrite loses
  to the cache. The persisted base is the answer to it, and the persisted base is
  where the invariants of `ctx/inject.rs` (I1, I4) apply.

## Next

0. ~~Blob test~~ and ~~stage 1 replay~~ are done (above). Next: wire the old
   blob drop (a flag, tail in messages), then Jev scoring over the stage 1
   output, and replace the crusher on reports with Jev stubs.
1. **Does size hurt?** On captured Spark sessions, compare turn latency and
   error or retry rate by context size. If 600k is no worse than 100k, stop.
2. **Offline score.** Take 3 captured Spark sessions. Score every unit against
   the ask that follows it with Jev, per unit. Read 50 dropped units by hand.
   Count how many the model went on to use (a later `headroom_retrieve`, a
   repeated read of the same file).
3. **Replay harness.** Apply a refresh to a captured session at the trigger and
   compare the next 20 turns against what the real session did.
4. **One session behind a flag**, background scoring, base persisted, fleet
   untouched.
