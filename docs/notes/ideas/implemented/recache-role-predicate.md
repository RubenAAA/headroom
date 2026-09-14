# Implemented: role-based scaffolding predicate in replay + drift

- **Status:** fixed 2026-08-26; validated 09-02 (`prefix_content_diverged`
  468k → 6.7k tokens/day, `early_messages` down too)
- **Source:** `docs/notes/recache-classification.md`
- **Summary:** 95% of `prefix_content_diverged` waste (1,998,513 tokens / 33
  turns) was withdrawn `role:"system"` messages; the old guard keyed on the
  optional `<system-reminder>` wrapper, but 81% of the 3,050 system-role
  messages are bare (same PreToolUse text sent both ways — tag can't be the
  test). `is_client_scaffolding_message` keys on `role` (Messages API never
  legitimately carries system-role in `messages`; index 0 excluded for
  OpenAI-Chat bodies). Replaying the withdrawal: 64→101 of 101 conversations
  survive. Accumulation watch reads clean (see
  `../recache-accumulation-watch-reread.md`).


## Detail

*moved from `docs/notes/recache-classification.md`*

## `prefix_content_diverged` — 95% of it was one predicate (2026-08-26)

The 2026-08-24 entry above closed this class at three events and two causes.
It reopened. Scoped by process start, the waste never went away:

| window | started | events | wasted |
|---|---|---|---|
| 18 | 2026-08-23 20:54 | 12 | 665,481 |
| 19 | 2026-08-24 12:02 | 8 | 200,538 |
| 21 | 2026-08-25 08:07 | 34 | 357,669 |
| 22 | 2026-08-26 06:54 | 19 | 467,348 |

Window 22 is the process running now, on the current build.

### One signature, and it is exact

Of 102 content-divergence declines since 2026-08-25, 88 splice and cost little.
The 14 that recover nothing are the same 14 three times over:

- `replayed_prefix_msgs == 0` ⟺ `chain_id == 0` — no held chain, so
  `replay_upto` is 0 and the whole stored prefix goes
- 12 of the 14 report `first_diff_path: role`, `diff_shape_stored: string`,
  stored head `system`, current head `assistant`

That is a withdrawn `role: "system"` message: every message behind it shifts by
one and the index-aligned compare meets an assistant where it stored a system.
**1,998,513 tokens over 33 turns, 95% of all `prefix_content_diverged` waste on
record** — 98% of it since 2026-08-25.

### Why the existing guard missed them

`align_over_withdrawn_scaffolding` was already there and already correct in
shape. It asked `is_pure_client_scaffolding`, which keys on the
`<system-reminder>` wrapper. The wrapper is optional.

Of the 3,050 `role: "system"` messages across the 114 stored prefixes, 593 carry
the tag and **81% do not**. The bare ones are output-style banners, `PreToolUse`
hook context, skill and agent listings and `Note:` file notices. Claude Code
sends the **same** `PreToolUse:Bash` text both ways — 468 tagged, 656 bare — so
the tag cannot be the test.

Counted 2026-08-26 against a store the proxy is still writing to, so the totals
drift between readings; the ratio holds.

`role` can. The Messages API carries the system prompt in a top-level field, so
a `role: "system"` entry inside `messages` never comes from the user or the
model. Index 0 is excluded: an OpenAI-Chat body puts its real system prompt
there, and losing that is a changed prompt. The proxy's own converters
(`handlers/gemini.rs`, `handlers/batch.rs`, `handlers/local_model.rs`) all push
theirs first, and none of the 114 stored conversations opens with one.

### Measured

`is_client_scaffolding_message` replaces the tag test in the replay comparator
and in the drift detector's early window. Replaying the withdrawal against every
persisted conversation:

```
conversations holding scaffolding: 101
  prefix survives the withdrawal BEFORE: 64
  prefix survives the withdrawal AFTER : 101
```

Reproduce with `price_the_role_predicate_against_persisted_conversations` in
`tests/early_reminder_drift_proof.rs`.

The cost of the blindness is unchanged and already documented above: replay
forwards the stored copy, so a withdrawn banner stays on the wire inside the
cached prefix at 0.1x. Watch `outbound_body_bytes` against
`client_request_bytes`; a ratio past ~1.2 means the accumulation is real.


## Before-column

*moved from `docs/notes/recache-classification.md`*

## Pre-restart baseline (2026-08-26)

Captured before the restart that puts the scaffolding predicate, the `BlockTag`
instrument and the transport `cause` fields into the running process. Nothing
below is live yet, so this is the "before" column and the only one that will
ever be measurable.

```
reason                                    all-time            2026-08-26
prefix_content_diverged        116 ev    2,108,921     20 ev      468,393
early_messages                  53 ev    1,129,413      3 ev      317,384
unexplained_after_replay       537 ev      527,269     71 ev       60,962
system                           9 ev      183,268      1 ev       34,549
tools                            5 ev      166,873      1 ev       67,512
concurrent_turn_in_flight      208 ev      150,652     44 ev       55,816
shorter_than_stored_prefix       3 ev       31,460      0 ev            0
system,early_messages           11 ev       12,479      0 ev            0
aftershock_of_diverged_prefix   15 ev        6,915      0 ev            0
TOTAL                          957 ev    4,317,250    140 ev    1,004,616
```

Read the all-time column with care: it spans several binaries. `86bd3fc2` added
the `origin` field partway through, so 42 events and 401,156 tokens before
08-24 carry a reason with no origin and cannot be split into client-caused and
proxy-caused. The 08-26 column is one day and one binary, and is the honest
comparison point.

Two predictions worth holding the authors to, both from the scaffolding
predicate:

- `prefix_content_diverged` should mostly go. 1,998,513 of its 2,108,921 tokens
  are the withdrawal the predicate now steps over.
- `early_messages` should fall a long way too, and this has *not* been priced.
  Its worst turn, 145,891 tokens on 08-26T08:28:32, is the same withdrawal seen
  from the drift detector's side, and the early-window filter now skips
  scaffolding. The top ten events hold ~930k of the 1,129k; the median is 1,349.
  If the number does not move, the filter is not reaching this path and that is
  the next thing to find out.

### Why the process now logs its own identity

Scoping a measurement "by process start" is the rule every number on this page
depends on, and it could not actually be followed. The log is appended across
restarts and reboots — five files, 22 runs — and carried exactly one
`headroom-proxy starting` marker in the current file, at 06:54:34Z, while the
process that wrote most of that file began at 07:26:23Z. Boot time checks out
(`/proc/stat` btime agrees with `/proc/uptime` to 0.09s), so this is not clock
drift; a run genuinely reached the log without announcing itself, and why is
still open.

`main.rs` now puts `pid`, `version`, `binary_len` and `binary_mtime` on the
starting line. Version alone cannot separate two builds of `0.1.0`; size and
mtime can. After the restart, "which binary produced this event" is answerable
from the log instead of from memory — which is how the counts on this page went
stale once already.


## 09-02 validation

*moved from `docs/notes/recache-classification.md`*

### The scaffolding fix holds

Against the 08-26 baseline above, `prefix_content_diverged` goes from 20 events
and 468,393 tokens in a day to 17 events and 6,699 — the events stay, the
tokens fall 70x, so what is left is tail churn rather than a rebuilt prefix.
`early_messages` goes from 3 events and 317,384 to 2 and 136,208. Both
predictions the last section asked to be held to are met.
