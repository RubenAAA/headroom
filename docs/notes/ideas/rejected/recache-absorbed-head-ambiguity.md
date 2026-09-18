# Idea: resolve the absorbed-head ranking ambiguity (declined — gate kept)

- **Status:** rejected 2026-09-17 — measured and declined. The hunt found 3
  live shape-(a) instances, 0 abandoned retries, and 0 counting impact from
  changing the gate (fall-through names the same waste in the same bucket).
  Numbers that killed the fix: 3 instances / 0 abandoned / 0 tokens moved.
  Follow-up (beta attribution) filed in
  `docs/notes/ideas/recache-blind-spot-watch.md`.
- **Source:** 2026-09-17 review of `recache_attribution` found two shapes:
  (a) empty inbound dims + `Some("")` outbound + `head_changed` still reports
  `prefix_head_changed`; (b) partial absorption keeps the absorbed dimension
  in a multi-dim reason string.
- **Value:** negative — avoiding a "fix" that deletes correct behavior. Shape
  (a) is exactly the abandoned-retry case (2026-09-03: dead request consumes
  the drift edge, billed attempt sees empty dims) where the head is the only
  surviving evidence; silencing it loses the one true cause those turns have.
  Shape (b) is cosmetic — the metric buckets any comma-joined reason as
  `multi` either way.
- **Next:** find a live `prefix_head_changed` with `drift_dims=""` and
  outbound `""`, check it against an abandoned retry on the same session. If
  it matches, document the shape as intended and close this; only a
  non-retry instance justifies changing the gate. Leave (b) unless a query
  needs the absorbed dimension split out.
- **Update 2026-09-17:** verified in code, still do-not-touch. Shape (a)
  gate: `client_edit_was_absorbed` (`usage_observer.rs:1102-1111`) returns
  false on empty inbound by design; head arm at `:1178` fires, comment
  `:1155-1177` cites the 2026-09-03 abandoned retry. Shape (b) cosmetic:
  partial overlap keeps the full inbound string, metric buckets comma
  strings as `multi` (`observability/recache.rs:120`). No non-retry
  instance, no query needing a split. Close query: `cache_recache_observed`
  with `attribution_reason=prefix_head_changed AND drift_dims="" AND
  outbound_drift_dims="" AND event_kind="drift"`, join each hit to
  `cache_drift_observed` on the same session hash in the prior ~60s.
  Companion work this round kept the `:1178` gate untouched: the new
  `head_moved` component field is logging only.
- **Measured 2026-09-17 (hunt done):** the close query over 98,914 JSON
  lines (5 processes, 08:04–13:26Z) returns **3 instances** — 09:50:28
  (88t), 11:10:35 (88t), 12:19:03 (15,171t, the noise-floor event), all with
  fused head `956403783fc5f7b2` and `forwarded_head_moved=0`. **Zero match
  the abandoned-retry shape:** every neighboring request completed
  (ledger-confirmed), cadence normal (15–30s), and zero `cache_drift_observed`
  edges on any of the three sessions at any time (3 drift events log-wide,
  none theirs). Gate change **declined anyway:** all three carry
  `replay_skipped=prefix_content_diverged`, so the fall-through names the
  same waste in the same Drift bucket — 0 counting impact, and the head fact
  is genuinely true. Shared fingerprint on all three: the client-rotated
  `anthropic-beta` header (`a06865b528e0`→`c660e3fc66b1`, proxy.rs:6550-6554)
  flipped exactly on the recache turn after 100–200 stable turns per
  session — provider-visible, both-lanes-quiet, and with **no arm in
  `recache_attribution`** (`beta_changed` rides along only as witness).
  Follow-up belongs to `recache-blind-spot-watch.md`: rank `beta_changed`
  as attribution evidence (ordering vs head/divergence TBD). This file can
  close once that follow-up is filed.
