# Learning: route Copilot at the generic host

- **Source:** `docs/notes/testing-copilot-subscription.md`
- **Claim:** `https://api.githubcopilot.com` serves the full model set
  (including newer models on Responses) and matches pre-0.23 working routing.
  Auto-selecting the per-account host from `/copilot_internal/user` regressed
  `wrap copilot` after 0.22.4 (#610) — segmented hosts don't serve newer
  Responses models and aren't what the official client uses.
