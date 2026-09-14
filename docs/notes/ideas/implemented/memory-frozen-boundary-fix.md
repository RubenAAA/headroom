# Implemented: memory frozen-boundary fix

- **Status:** fixed (pinned by `tail_anthropic_reaches_a_short_conversation`)
- **Source:** `docs/notes/proxy-experiments-2026-08.md` (offload-gap round)


## Detail

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **A separate memory bug, fixed on the way.** The memory injection site computed
  `frozen` as the length of the **system** array and passed it to
  `append_to_latest_user_tail` as `frozen_message_count`, which indexes into
  **messages**. Two system blocks skipped `messages[0..2]`, so a conversation one
  or two messages long got no memory, silently. It now passes 0, which is honest:
  the real frozen boundary comes from the prefix-replay tracker, which does not
  run until `apply_prefix_replay`. Pinned by
  `tail_anthropic_reaches_a_short_conversation`.
