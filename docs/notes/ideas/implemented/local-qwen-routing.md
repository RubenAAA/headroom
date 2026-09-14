# Implemented: local Qwen model routing (generalized same day)

- **Status:** done 2026-07-03 (`a4ea71f4`, generalized `fe5a3454`)
- **Source:** `docs/notes/plans/2026-07-03-local-qwen-model-routing.md`
- **Summary:** `local_model`/`local_upstream` config + handler + `/v1/messages`
  route + `integration_local_model.rs`. Handler has since grown far past the
  plan (routing fallback, CCR tracking, cache stabilization) — the plan doc is
  reasoning history, not current behavior.
