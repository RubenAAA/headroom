# Learning: key scaffolding on role, not on markup tags

- **Source:** `docs/notes/recache-classification.md` (95% predicate, 2026-08-26)
- **Claim:** of 3,050 `role:"system"` messages in stored prefixes, 81% are bare
  (banners, hook context, listings) — and Claude Code sends the same
  PreToolUse text tagged and untagged (468 vs 656), so the
  `<system-reminder>` wrapper cannot be the test. `role` can: the Messages API
  never legitimately carries system-role inside `messages` (index 0 excluded
  for OpenAI-Chat bodies).
