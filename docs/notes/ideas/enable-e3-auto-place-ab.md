# Idea: enable gated E3 auto-placement and measure

- **Status:** open (code shipped `8672d5c3`; gated OFF for our traffic —
  `e3_skipped{reason: auth_mode}` baseline)
- **Source:** 2026-09-18 session; `anthropic_cache_control.rs:252-280`
- **Value:** one marker on the last tool makes marker-less bodies cacheable;
  after the first write it is stable (`MarkerPresent` forever after). Our
  client (Claude Code) places its own markers, so the win is concentrated on
  naive-client traffic — size that slice first.
- **Intervention:** same two options as `enable-e1-e2-sort-ab.md` (PAYG canary
  preferred; enforcement-disabled is confounded). Can run in the same window
  as E1/E2 — different events, same ledger.
- **Track:** `e3_applied` (with `placed_count`/`locations`) vs `e3_skipped` vs
  `e3_no_target`; read-rate lift on bodies that previously had zero markers;
  drift waste; any upstream safety signal.
- **Exit:** keep on if previously-uncacheable bodies gain reads with no
  safety signal; otherwise reject with the number. Note: for traffic that
  already carries client markers this is a permanent no-op — that slice
  converging to `MarkerPresent` skips is success, not failure.
