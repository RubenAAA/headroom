# Idea: Linux secret-store auto-discovery coverage for Copilot subscription

- **Status:** open (needs a volunteer with 10 min + a Copilot seat)
- **Source:** `docs/notes/testing-copilot-subscription.md`
- **Summary:** macOS Keychain + Windows device-auth paths verified; Linux
  `secret-tool`/libsecret auto-discovery untested, and Windows Copilot-CLI
  credential reuse confirmed absent (1.0.81 exposes no legacy schema → use
  `headroom copilot-auth login`). `GITHUB_COPILOT_TOKEN` bypasses discovery.
- **Next:** host-native install, `headroom wrap copilot --subscription -- --model
  gpt-4o -p "Reply with exactly: HEADROOM_OK"`; on miss, report redacted
  `secret-tool search --all` attribute lines + whether the env-var retry worked.
