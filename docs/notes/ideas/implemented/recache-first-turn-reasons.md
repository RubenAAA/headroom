# Implemented: first-turn writes get a reason

- **Status:** done 2026-09-02
- **Source:** `docs/notes/recache-classification.md`
- **Summary:** the classifier scored only against previous turns, leaving first
  turns (41% of all cache-write tokens) unscored. Now emits
  `first_turn_write_observed` with precedence-ordered reasons
  (`compaction_restart` / `session_key_drift`+donor / `identical_prompt_fanout`
  / `fresh_session` / `arrived_with_history`), summed in
  `proxy_cache_first_turn_write_tokens_total{reason}`.


## Detail

*moved from `docs/notes/recache-classification.md`*

### First turns are the largest write, and they are avoidable

215 new sessions opened in the window. Their first turns account for 7.5M
tokens, 42% of all cache writes at the time of that pass. `system` and `tools`
are shared and read back at ~17k; the write is the first user message — ~24k
for a one- or two-message opener, ~95k for the 28 sessions first seen
mid-conversation.

The `<system-reminder>` block carrying `CLAUDE.md` in message 0 is ~47 KB and
byte-identical across sessions of the same project, so it should be read, not
written. Recall injection prepends session-specific bytes ahead of it and
spoils that. Fix in progress: place recall after the scaffolding and put the
breakpoint on the scaffolding block.


## Reason spec

*moved from `docs/notes/recache-classification.md`*

## First-turn writes get a reason (2026-09-02)

The classifier scores a turn only against a previous turn under the same
conversation key, so the first completed turn of every key went unscored.
Those first turns are 41% of all cache write tokens (11.1M over 09-01..09-02).
The observer now emits one INFO event, `first_turn_write_observed`, when the
first completed turn under a key writes more than `RECACHE_SLACK_TOKENS`, with
`attribution_reason` from a fixed set, in precedence order:
`compaction_restart` (message 0 carries Claude Code's compaction summary),
`session_key_drift` (the replay store found a donor tracker under another
session key; `adopted` and `donor_session_key_hash` ride along),
`identical_prompt_fanout` (an opener of two or fewer messages whose message-0
hash was seen under another key inside 10 minutes: parallel subagents),
`fresh_session` (two or fewer messages, nothing else applies), and
`arrived_with_history` (more than two messages: new content for the provider).
The handler computes the message count, message-0 hash, and compaction marker
once and parks them on the pending request. Tokens are summed per reason in
`proxy_cache_first_turn_write_tokens_total{reason}`.


## Tail confirmation

*moved from `docs/notes/recache-classification.md`*

- **`first_turn_write_observed`: live.** ×33 in the window:
  `fresh_session` 20, `session_key_drift` 7, `compaction_restart` 6,
  `identical_prompt_fanout` 0, `arrived_with_history` 0. Donor adoption is
  active (the 7 drifts) — this event is now the instrument to watch the P1
  gate-seeding work with.
