# Idea: make first turns read the shared prefix instead of writing it

- **Status:** open (mechanism mapped 2026-09-17; revised twice same day after
  review — round 1 corrected the prize, D4 gate, hierarchy order, and size
  threshold; round 2 below corrects the D4 estimate basis, reason totals,
  ledger accounting, and the D-tools claim; all corrections verified, not
  assumed)
- **Source:** 2026-09-17 investigation starting from `prod 84%` → first-turn
  write share, plus same-day review. Code: `prefix_replay.rs`
  (`place_tail_cache_breakpoints`, `opening_scaffolding_target`,
  `message_slots_within_budget`), `ctx/inject.rs`
  (`leading_scaffolding_len`), `usage_observer.rs`
  (`first_turn_write_observed`, `first_turn_reason`, `complete()` billing
  basis), prior `learnings/first-turn-write-share.md`. Guarding test:
  `tests/integration_shared_scaffolding_prefix.rs:184`
  (`two_sessions_share_the_opening_scaffolding_prefix`) — existing scaffold
  sharing must keep passing.
- **Value:** small and now honestly sized. Window 08:04:45Z→10:20:57Z on
  2026-09-17: 26 FirstTurn classifications; ledger first-turn writes 466,709
  tokens, of which `first_turn_write_observed` (outer prompt only, see below)
  counts 387,952 over 23 turns. No savings projection is approved — not even a
  micro-estimate — until a 24-hour shadow calculation or A/B measures the
  actual read/write shift. The narrow D4 trial is approved on risk grounds
  (near-zero cost), not on a promised return.
- **Next:** implement D0 with the counterfactual fields below; trial narrowly
  gated D4; simulate the tools-breakpoint tradeoff for compaction turns.

## Baseline, exactly

The original draft combined two different costs at former line 10. They split
as follows — verified by joining `turn_cost_ledger` to
`first_turn_write_observed` and `ccr_continuation_usage` per request id:

- `turn_cost_ledger.cache_creation_input_tokens` is **billed totals including
  hidden continuation rounds**: the ledger substitutes
  `pending.billed_totals` (`usage_observer.rs:2033-2041`, fed by
  `note_billed_totals` at `proxy.rs:10688-10704`). `first_turn_write_observed`
  fires inside the same `complete()` call but uses the call's outer-response
  arguments — `cache_baseline_*` with continuation rounds explicitly removed
  (`proxy.rs:10677-10687`: "the right thing to classify against and the wrong
  thing to bill"). Same function, different counters — that distinction is
  precisely why request `96a4732e` differs.
- 70,651 tokens (request `cf86c94a`, the 390K-read arrived-with-history turn)
  never appear in `first_turn_write_observed`: that branch requires
  `streams_tracked == 0`, and the turn matched a tracked stream. It is the
  window's single biggest first-turn write and currently invisible to the
  first-turn counters — a D0/D3 measurement gap, not retrieval rounds.
- 8,106 tokens (request `96a4732e`) are hidden CCR-round writes on a first
  turn: the single observed request where ledger exceeds observed, joining
  exactly to its `ccr_continuation_usage.cache_write_tokens`.
- The `> RECACHE_SLACK_TOKENS` (64t) floor excludes at most ~26×64 ≈ 1.7K tokens.

So of the 78,757 ledger-minus-observed gap: 70,651 tracked-gate exclusion,
8,106 retrieval rounds, ~0 floor effect. D4/D2 operate on the outer
first-turn prompt; the rounds portion belongs to the repeat-retrieval loop
fix, not here. All counts below state window 08:04:45Z→10:20:57Z and use the
outer-prompt series unless marked billed.

Reasons on the observed basis: `fresh_session` 266,607 (16 turns),
`compaction_restart` 120,121 (6), `session_key_drift` 1,224 (1).
Total: 387,952 over 23.

## The rule everything follows

Provider cache order per Anthropic docs is **tools → system → messages**
("Cache prefixes are created in the following order: tools, system, then
messages"; breakpoint hashes are cumulative, so a change at or before a
breakpoint re-hashes it). Consequences, all verified against the window:

- Message variance poisons only the message span: same-pair sessions sharing
  sys+tools+scaffold read everything but the task tail (three same-pair
  sessions minutes apart — first writes 10.5K, next two read 7.9K).
- A system breakpoint represents the cumulative tools+system prefix, not the
  system alone. A same-system match is insufficient if tools differ, and
  eligibility depends on cumulative tools+system tokens. Four turns shared
  exact tools+system bytes, but one was Sonnet and three Opus — only the three
  Opus turns form one cache lineage, giving one cold write plus two potential
  hits. (A fifth no-system-marker Opus turn shares the system with different
  tools: nothing cacheable in common with that pair.)
- System variance poisons system+messages but **not** a tools span placed
  before it. The original draft killed the tools breakpoint on the wrong
  mental model (system-first); it is reopened below.
- Only head-anchored shared spans are shareable, and the tools span depends
  on nothing before it — making it the cheapest span to share once marked.

Zero `identical_prompt_fanout` all window: no two sessions shared message-0
bytes within 10 min. Cardinality is high: 12 distinct (sys,tools) pairs over
24 fingerprinted turns (+2 Codex turns without fingerprints), rosters 6–35
tools.

## The design (ranked, re-gated)

**D0. Instrument first — implemented 2026-09-17, reading now.** New event
`first_turn_prefix_diagnostic`, emitted for *every* turn classified FirstTurn
with none of the write branch's gates — covering tracked first turns (the
`cf86c94a` hole) and tiny turns. Pure observation: no counter moves, the
fan-out table is untouched, and `reason` is derived offline (message-zero
hash joins across conversations) rather than recomputed against live tables.
Per line: request-path cache-key controls (`tool_choice`, `thinking`,
`effort`, images-in-message-0, `opens_with_scaffolding`, message-0
scaffold/rest byte sizes), outer vs rounds usage split (input/read/write
plus the 5m/1h tier split), model/msgs/compaction/adoption/replay flags.
Deliberately joined, not self-contained: marker layout rides on
`turn_cache_fingerprint`, forwarded sys/tools hashes on `prefix_composition`,
beta/auth digests on the former — all keyed by request_id. Eligibility,
entry availability/TTL, and counterfactual savable tokens are computed
offline (span bytes/4 from `prefix_composition`, per-model minimum table,
entry table from billed ledger writes), keeping provider thresholds out of
the code. Document the exact measurement window with every reading.

**D4 (narrow). Trial a system marker only when all four hold: first turn AND
no system breakpoint on the final layout AND a genuinely free marker slot
(counted post-trim, not assumed from `streams_tracked == 0` — 19 of 24
fingerprinted first turns already use all four slots) AND the model-specific
minimum is met on the cumulative tools+system prefix.** The known population
today is 5 single-marker no-sys turns. No projection is attached: the earlier
~4,700 figure assumed three hits on system bytes alone, but only two
follow-up turns share the exact pair (one cold write plus two potential
hits), and eligibility is cumulative — D0 measures the actual tools+system
span first. Approve the trial on risk grounds, not on a promised return.

**D-tools (reopened, claim narrowed).** A tools breakpoint does **not** read
"whenever tools bytes match": model, cache namespace/auth, beta headers,
tool_choice, thinking configuration, effort, image presence, and other
provider controls can all invalidate it — and no tools-only checkpoint is
ever written today (writes happen only at breakpoints, so the tools bytes are
only ever inside the cumulative system checkpoint). Supporting evidence for
the gap: three tiny sessions sharing one exact sys+tools pair with differing
message-0; repeated pairs among compaction restarts (one ×3, one ×2); several
sys variants sharing one tools fingerprint. Tiny sessions have spare slots
for a tools → system → m0 ladder. Compaction turns are slot full: simulate
the marker tradeoff separately before touching them.
The window's sharpest unexplained case is D0's prime diagnostic, not D-tools
evidence: 09:29 (`a3c587cc`) and 09:50 (`6d22db62`) share identical logged
model, system, tools, beta, and auth digests — yet 09:29 read 247,495 /
wrote 1,224 while 09:50 read 3,470 / wrote 51,147. History cannot explain it:
message bytes sit downstream of the system checkpoint both turns already
carry, so the cumulative tools+system entry should have matched at 09:50
unless a global control, cache namespace, or entry TTL (including a silent
5m downgrade of the source entry) differs. D0's full-controls capture plus
entry availability/TTL exists to name that decider.

**D3. Compaction restarts — measure, don't accept yet.** 120K, but the
repeated-pair cases above mean recoverable value exists, and the window's
biggest first-turn write (70,651 tokens) is an arrived-with-history turn the
counters never see. Check sys/tools span reads per compaction turn first.

**D2. System-variance audit — behavior-changing work, not free upside.**
Moving cwd, role, or timestamp material can alter prompt semantics; diff
same-model system bodies, then review each normalization for meaning change
before costing it. Note the relationship to D-tools: a tools breakpoint
already survives system variance on its own, so D2 does not unlock tools-span
reads — it extends a surviving tools read through the system span (and
potentially into messages) wherever a collapsed variant lets the system
checkpoint hit too.

**D1. Sort tools on first turns — conditional.** Only 1 tool-set appears in 2
orders; a wrong sort breaks arrived-with-history reads. Proceed only if D0
shows order variance among first turns.

**Still killed:** request pacing (refuted: recache rate flat vs idle gap);
5m TTL (loses on subscription per `learnings/ttl-policy-gap-66pp.md`);
marker-text edits (risk, no prize).

## Thresholds

The original draft treated 1024 tokens as universal. Current Anthropic
minimums are per-model: **512 for Opus 5, 1024 for Sonnet 5** (non-monotonic
across generations — verify per model at implementation time, do not hardcode
one constant). What D4 eligibility measures is the cumulative tiny-Opus
tools+system prefix (~8KB, ~2K tokens), not the 3.3KB system alone; the
Sonnet equivalent stays uncertain. Byte size alone cannot establish
eligibility — gate on the proxy's token accounting. Reference for any future
estimate: 4-breakpoint limit, 2.0×/0.1× 1h write/read pricing.

## Re-measurement

Window every reading (current figures: 08:04:45Z→10:20:57Z). Outer vs billed:
join `turn_cost_ledger` (billed, incl. rounds) to `first_turn_write_observed`
(outer, gated on `streams_tracked==0` and >64t) and `ccr_continuation_usage`
(rounds split) per request id, as in this investigation. Sibling loop
baseline: calls-per-hash from `ccr_retrieval_call` (2026-09-17 window:
220/63 ≈ 3.5).

Automated daily reading: `~/.local/bin/headroom-d0-monitor.py` (stdlib only),
cron `30 5 * * *` with `--hours 24 --append`; history in
`~/.headroom/d0-readings.jsonl`, run log in `~/.headroom/d0-monitor.log`. It
reports D0 coverage (classifications with diagnostic lines — expect ~100% now
that the emitting binary is live), offline reasons, entry-availability
answers, D4/tools-ladder candidates with savable-token estimates, calls per
hash with trend vs the prior reading, and current prod%. First baseline
appended 2026-09-17 (coverage 2/29 — the rest predate the D0 binary).
