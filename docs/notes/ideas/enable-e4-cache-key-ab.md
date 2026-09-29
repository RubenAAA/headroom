# Idea: enable gated E4 prompt_cache_key and measure

- **Status:** open (code shipped `573543fc`; gated OFF for our traffic).
  Re-confirmed 2026-09-21: `e4_skipped` fired 7,125 times over six days,
  all `reason=auth_mode` on `/v1/responses`. Gate-blocked, not
  traffic-blocked.
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

## 2026-09-29 evidence for the subscription Codex route

Correction: the routed subscription path is not keyless. `openai/request.rs`
sets `prompt_cache_key` to the raw `metadata.user_id` JSON (device id, account
uuid, session id), which differs from the `session-id`/`thread-id` header the
Codex CLI uses for both. The E4 gate only skips the synthesised key.

On 2026-09-25, 42 of 233 routed Codex turns read under half their prompt,
mostly falling to the 15,104-token instructions prefix; they account for 2.91M
of the 5.02M "recache waste" tokens in the 09-24..29 logs (see
`recache-provider-reasons.md`). Miss rate rises with the gap since the
session's previous turn (about 9% under 10 s, 24% under 60 s, 86% at
120-300 s), which suggests short backend retention or routing rather than key
absence.

Shipped 2026-09-29 (`routed/translation.rs`): on the ChatGPT-subscription
route the key is `derive_session_uuid(user_id)`, the same value as the
`session-id` header, and account ids no longer go upstream. Each live session
takes one miss when the key changes. Untested: whether this moves the miss
rate, and whether `x-codex-window-id` / `x-codex-routing-hint` (not sent) matter.
To measure: routed-turn miss rate by gap bucket before and after a restart.
