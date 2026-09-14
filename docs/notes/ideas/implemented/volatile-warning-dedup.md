# Implemented: volatile warning fires only on value change

- **Status:** fixed and verified 2026-08-26; first-sighting demoted to INFO
  (`volatile_content_suspected`) 2026-09-01
- **Source:** `docs/notes/proxy-followups.md` §1
- **Summary:** shape-only detection was 86% noise (147 constant groups of 171;
  worst a fixed uuid warned 10×). Now keeps last value per
  (conversation, location), warns only on change; intra-request finding sets
  judged as a set. Every constant group fires exactly once, then never again.
- **Residue:** one `uuid_v4` per site keyed on message index — position-keyed,
  not worth tightening.


## Detail

*moved from `docs/notes/proxy-followups.md`*

## 1. Volatile-content warning fires mostly on constants

**Status: fixed and verified 2026-08-26.** The suppression described at the
bottom of this item ships in `volatile_detector::emit_one`, keyed on a
`sample_digest` per `(conversation, location)`. Re-measured over the process
that started 2026-08-26 06:54: 368 findings, 254 groups, 221 of them
constant-valued — and **every constant group fired exactly once**. Zero repeat
warnings. What is left is one first-sighting per location, which is the
documented floor: nothing has been observed yet, so nothing can be compared.

**Amended 2026-09-01.** That floor was still too loud, and worse, it was
saying something it could not know. Over the process that started 2026-08-30:
589 warnings, 81 locations seen with more than one sample — and every one of
those samples came from a *single request*, a block holding several dates.
Grouped by `request_id`, not one location was ever observed changing between
turns. So every warning in that window was an unconfirmed first sighting
calling itself `volatile_content_detected`.

First sightings now report at INFO as `volatile_content_suspected`, and WARN
means the value was seen to move. The 2026-08-26 reason for keeping the first
sighting holds — a one-request conversation must still hear something — but it
is answered by the INFO line, not by a warning. The proxy runs at
`--log-level info`, so nothing is lost.

The residue is a `uuid_v4` inside `tool_result` content, and it reads as a new
group each time only because the location carries the message index
(`messages[9]`, `messages[15]`, `messages[21]`). Tightening that further means
keying on something other than position, and it is not worth it at one warning
per site.

**Closed 2026-09-11.** Zero WARN `volatile_content_detected` across the ~10 h
window (6,465 INFO `suspected` sightings, none ever confirmed moving). The
"not worth it" call above made itself: the residue fires nothing. Leave the
detector as is.

120 warnings an hour. The detector flags a value by its *shape* — a
uuid or an ISO timestamp inside the cached prefix — and never checks
whether that value actually changed between turns. A constant uuid in
message 0 costs nothing and is warned about as loudly as a clock that
ticks every request.

Joining findings on `(conversation_key, kind, location)`, which the
`conversation_key` field now makes possible:

| | groups |
|---|---|
| value changed across turns (real cache buster) | 24 |
| value constant (false positive) | 147 |

318 findings, 171 groups, **86% noise**. The worst offender is
`uuid_v4` at `messages[0].content[1].text`: 10 findings, **one**
distinct value, seen across 9 different conversations — a fixed string,
not per-request churn.

The real ones are mostly timestamps: of the 24 changing groups, 16 are
`iso8601_timestamp` and 8 `uuid_v4`.

Fix: keep the last value seen per `(conversation, location)` and warn
only when it differs. Shape alone is not evidence.


## Fix 4

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **4 (and partly 3a/5/11):** `volatile_content_detected` now carries
  `session_key_hash` and `conversation_key`, the same two identifiers the drift
  and recache events use, so a volatile finding can be joined to the bust it is
  suspected of causing. Item 4's caveat — "the warning logs no session key, so
  1811 varying is an upper bound" — is now answerable from logs. The session key
  is derived once per request and shared with the drift detector rather than
  re-derived, which would have put six extra SHA-256 digests on the hot path of
  every request.


## Fix 4 emit

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **4 (fix):** `emit_volatile_warnings` remembers what each location last held
  per conversation and warns only when the value moves. Two details that
  decide whether it works at all: findings at one location within a request are
  judged **as a set** (a docstring with five example dates arrives as five
  findings under one path, and compared one at a time each differs from the one
  before, so every location would look like it was churning); and the first
  sighting still warns, because a conversation that only ever sends one request
  — most subagent traffic — would otherwise never hear from the detector.
  Suppressing the first sighting too was tried and dropped for that reason.
  Memory is a bounded LRU, 256 conversations by 64 locations.
  *Superseded 2026-09-01:* the first sighting reports at INFO as
  `volatile_content_suspected` and no longer warns. See item 1 of
  `proxy-followups.md` for the measurement that forced it.


## Item 4

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 4. Volatile-prefix warning fires on static values

**DECIDED AND FIXED 2026-08-09.** The caveat below — "1811 varying is an upper
bound" — is now answerable, and the answer is that **nothing varied**. Grouping
the live warnings by `(conversation_key, location)` and comparing the sample
set per request: 37 sites seen in two or more requests, **zero** of them ever
changed value, 4,004 warnings between them. Fix (a) is implemented; see the
fixes section. Note this also disposes of the shape of the original count —
"825 come from values that never change" was itself an undercount, because
several samples at one location within a single request were being read as
change over time.

2455 warnings in 67 minutes, 7.2 per request — 43% of all log lines this run.

Tested by collecting distinct samples per location: **825 warnings come from
values that never change once.** Every `tools[].input_schema` hit is the
literal string `2026-07-14T10:05:00` in a `from.description`; placeholder
UUIDs like `22222222-2222-4222-a222-222222222222` also recur verbatim.

The detector matches on shape ("looks like an ISO timestamp") rather than
diffing what it saw last time. Static example text in a tool docstring cannot
bust a cache.

Meanwhile measured `cache_hit_pct` is **99 median, 92.7 mean** — the failure
the warning predicts is not happening at anything like this rate.

**Caveat:** the warning logs no session key, so for the 12 locations whose
samples do vary I could not separate per-session churn from ordinary
cross-session difference. 1811 "varying" is an upper bound, not a count.

**To investigate:** (a) suppress when the value at a location is unchanged
from the previous request on the same conversation; (b) add a session key to
the warning so this is answerable from logs. Note the drift detector only
hashes system, tools and the first 3 messages — open question in
recache-classification.md about widening that window is related, since
the busiest locations here are `messages[3]`, `[6]`, `[13]`, `[38]`.

**Source.** Emitted by `emit_volatile_warnings`
(`cache_stabilization/volatile_detector.rs:155-168`), message text at :163-165.
Detection runs in `detect_volatile_content` (:143-150) walking the body via
`walk_anthropic` / `walk_openai` (from :172); the `VolatileKind` variants
`Timestamp` / `Uuid` / `IdField` are at :68-92. Confirmed by reading: the
detector matches shape only and holds no previous request to diff against, so
fix (a) needs new state, not a new condition.
