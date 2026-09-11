# Idea: P3 headroom-policy — the PDP (Enterprise, license-gated)

- **Status:** open (gated: after P2; IP gate cleared 2026-09-11 — owner-confirmed personal use)
- **Source:** `docs/context-mode-integration-analysis.md` §4 P3
- **Summary:** `security.ts` as a policy decision point (org rulesets,
  containment, shell-escape detection, tamper-evident audit) at two attach
  points: P2's hook layer and `pipeline_extension` PRE_SEND. Enterprise-only
  features; gate with ELv2 key. Medium effort — engine tested, work is the
  control plane.


## Proposal

*moved from `docs/context-mode-integration-analysis.md`*

### P3 — `headroom-policy` (Enterprise, license-gated): the PDP

**What:** `src/security.ts` as a policy decision point, plus centrally-managed org rulesets.

Two attach points: the hook layer from P2 (tool-level `allow/deny/ask`), and
`headroom.pipeline_extension` at `PRE_SEND` (prompt-level policy). Feeds `headroom/audit/`.

**Enterprise features that only make sense paid:** central policy service, org-wide allow/deny
rulesets, project-boundary containment enforcement, shell-escape detection inside sandboxed code,
tamper-evident audit trail, per-team reporting. Gate it with the ELv2 license key (see §6).

**Effort:** medium. The engine exists and is tested (`tests/security/`, `src/security.ts` 889 lines);
the work is the control plane.
