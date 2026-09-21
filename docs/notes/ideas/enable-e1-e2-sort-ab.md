# Idea: enable gated E1/E2 sort passes and measure

- **Status:** open (code shipped `4a3b76bc`/`9112fed9`; gated OFF for our
  traffic — triple-skip is the current baseline). Re-confirmed 2026-09-21
  over six days: `e1_skipped` and `e2_skipped` fired on **31,008 of 31,008**
  requests, every one `reason=auth_mode, auth_mode=subscription`. This is
  gate-blocked, not traffic-blocked — no amount of waiting produces a
  measurement, only a PAYG canary or a policy-enforcement window will.
- **Source:** 2026-09-18 session; `tool_def_normalize.rs:70-95,207-234`,
  `live_zone_anthropic.rs:601-652`
- **Value:** deterministic tool/schema order should cut prefix re-keys (tools
  ≈ 46% of prefix mass). Currently unproven on live traffic because the
  PAYG-only gate skips 100% of our Subscription/OAuth requests.
- **Intervention (pick one):** (a) PAYG canary — route a slice of PAYG-keyed
  traffic with enforcement on; (b) `--auth-mode-policy-enforcement disabled`
  in a quiet window (coarse: also flips compression/headers/injection, so
  isolate the window and note the confound).
- **Track:** `e1_applied`/`e2_applied` rate vs `e1_skipped`/`e2_skipped`;
  drift-waste tokens (depth-binned, first turn dropped, per
  `recache-counting-rules.md`); cache-hit rate on the tools prefix; any
  upstream scope/revocation signal (the reason for the gate — stop on first).
- **Exit:** ship (leave on) if drift waste drops with no safety signal;
  reject with the number if neutral/negative; reverts to gated-skip.
