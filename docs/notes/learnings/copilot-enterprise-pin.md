# Learning: enterprise Copilot hosts pin via env, not auto-detect

- **Status:** auto-detect from the token-exchange endpoint is a wanted
  contribution (needs a real enterprise tenant to validate)
- **Source:** `docs/notes/testing-copilot-subscription.md`
- **Claim:** dedicated-host tenants (GE Cloud data residency, egress proxies)
  must set `GITHUB_COPILOT_API_URL` explicitly — the override flows through
  both `--subscription` and OAuth to the upstream request.
