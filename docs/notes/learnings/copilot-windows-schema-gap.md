# Learning: Copilot CLI 1.0.81 exposes no reusable Windows credential

- **Source:** `docs/notes/testing-copilot-subscription.md` (live Windows testing)
- **Claim:** `cmdkey /list` shows no Copilot target on 1.0.81, so native
  credential reuse is unavailable — use `headroom copilot-auth login`
  (Headroom device auth, verified). Report only `Target:` lines, never secrets.
