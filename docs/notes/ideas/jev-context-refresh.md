# Idea: Jev-guided context refresh in place of client compaction

- **Status:** open, unbuilt, and it needs a lot of tuning before it can run on a
  live session (see "Tuning"). The first question is whether big contexts hurt
  on Spark at all; nothing here measures that.
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
