# Learning: ELv2 is a hard blocker, and the plugin boundary is the answer

- **Source:** `docs/context-mode-integration-analysis.md` §6 + §8
- **Claim:** context-mode is Elastic-2.0 (Mert Koseoglu) vs Headroom Apache-2.0 —
  merging relicenses core, and ELv2's hosted-service clause hits
  `headroom-managed/` (unlicensed SaaS arm) hardest. Separately-licensed
  `plugins/` packages (the `headroom-oauth2` shape) keep core clean, and ELv2
  suits a key-gated enterprise tier. Needs an IP arrangement in writing before
  any code — plus the realignment-collision and Phase-H-direction gates.


## SaaS-arm sharpening

*moved from `docs/context-mode-integration-analysis.md`*

**`headroom-managed/` is the SaaS arm, and it is unlicensed.**
`headroom-managed/pyproject.toml`: `name = "headroom-managed"`, `description = "Headroom SaaS
Platform - Managed context window optimization"`, `version = 0.1.0`. It has `app/auth.py`,
`app/middleware/`, `app/routes/`, `app/services/`, `app/models.py`, alembic migrations, and a
`pilot/`. There is **no `license` field and no LICENSE file** — i.e. proprietary by default.

This *sharpens* the §6 blocker rather than easing it. ELv2 forbids providing the software "to third
parties as a hosted or managed service." The product whose name is literally *Managed* is the one
place context-mode-derived code cannot go without an explicit commercial grant from the copyright
holder. Plan the plugin boundary so that `headroom-managed` consumes only Apache-2.0 core
interfaces, never ELv2 implementations.
