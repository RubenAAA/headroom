# Idea: enable gated E4 prompt_cache_key and measure

- **Status:** open (code shipped `573543fc`; gated OFF for our traffic)
- **Source:** 2026-09-18 session; `openai_cache_key.rs:146-149,418-442`,
  caller `proxy.rs:10196`
- **Value:** deterministic `(model, system, tools)` key pins OpenAI
  prefix-cache lookup; one field-add, constant across turns, no recache by
  construction. Cheapest experiment of the set — key is turn-invariant, so
  the only question is hit-rate lift, not stability.
- **Intervention:** PAYG canary on OpenAI-shape traffic (Chat/Responses).
- **Track:** `e4_applied` vs `e4_skipped{KeyPresent|auth_mode}`; OpenAI cache
  hit/read rate before/after on the canary slice; confirm key stability
  (same system+tools ⇒ same key across turns).
- **Exit:** keep on if hits lift with stable keys; reject with the number
  otherwise. No byte risk beyond the single injected field.
