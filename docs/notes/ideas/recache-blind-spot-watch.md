# Idea: exonerate or indict the classifier blind spots

- **Status:** beta half shipped and ranked; model-flap witness shipped
  log-only (unranked, zero instances); TTL + tail exonerated
- **Source:** 2026-09-17 code-reading proof that several provider key inputs
  are invisible to both drift lanes: beta headers (hash reads body only),
  `cache_control`/TTL moves (stripped), model-route changes (router runs
  after the fingerprint), tail compression/offload nondeterminism (past the
  `early[0..3]` window). Beta/marker moves now ride the event as
  `beta_changed`/`markers_changed` (`note_forward_witnesses`, called from the
  `turn_cache_fingerprint` stage).
- **Value:** either promotes "the proxy pipeline doesn't bust its own cache"
  from asserted (41 quiet pairs, one byte-stable window) to measured — or
  produces the first real instance, which names its own fix.
- **Next:** accumulate `beta_changed`/`markers_changed` on unexplained events;
  join positives to `beta_header_merge` and `messages_rewritten` on
  `request_id`. Deliberate gap: forwarded-model route flap is still
  unwitnessed. If otherwise-clean misses (all flags false) persist, add a
  forwarded-model witness on the `note_forward_witnesses` pattern *before*
  inventing new causes.

## First positives 2026-09-17 (no proxy cause possible)

Two unexplained events with beta AND markers both unchanged — nothing
moved that anyone can see, yet the provider dropped older entries:
14:28:55 `dropped_older_entry` (34,816 tok) + 14:29:16
`free_read_not_persisted` (36,864 tok), conv `5c91dc42`, both
`commit_race_suspect=true`. At 71.7k tokens (8% of the day's residual
waste in 2 events) these are the strongest pure-evication specimens so
far: replay applied, key stable, completions recent — and the older
footprint gone anyway. They exonerate every proxy transform at once
(no flap to find) and bound what any blind-spot hunt can ever explain.

- **Measured 2026-09-17 (first real beta instances — from the
  absorbed-head hunt):** 3 `prefix_head_changed` drift events (09:50:28 88t,
  11:10:35 88t, 12:19:03 15,171t) share one pattern: forwarded
  model/system/tools digests byte-stable, both drift lanes quiet, and the
  client `anthropic-beta` header flipped (`a06865b528e0`→`c660e3fc66b1`)
  exactly on the recache turn after 100–200 stable turns per session. Beta
  is client-supplied (proxy.rs:6550-6554) and provider cache-key input, so
  the rotation is a genuine bust cause `recache_attribution` cannot name —
  `beta_changed` rides along only as witness. Concrete next step: rank
  `beta_changed` as attribution evidence (ordering vs `prefix_head_changed`
  / `prefix_content_diverged` TBD — all three were simultaneously true on
  these turns); no counting change, label only. See
  `rejected/recache-absorbed-head-ambiguity.md` for the full hunt — and the
  exonerations below (TTL all-1h, tail ladder stable on 58/58 residual
  turns, `beta_header_merge` 0-joined, 9 router rewrites all 403-fallback).
- **Shipped 2026-09-17 (model witness):** `forward_model`
  (post-router string) parked by `note_forward_witnesses`, compared
  turn-apart under the both-known rule, emitted as `forward_model` +
  `model_changed` on the drift and unexplained arms
  (`usage_observer.rs`) — witness only, `recache_attribution` untouched
  (zero flaps in-window: 0/60 multi-model conversations). Drive-bys with
  it: `event = "model_routing_decision"` (+ from/to model) on the router
  line (`model_router.rs`), which was invisible to event queries. Residual
  redefined (strict-clean was 0 by artifact — `markers_changed` 99.5% FP
  from normal growth): beta-stable + content-stable = 58 events /
  ~172kt. `markers_changed` stays unranked permanently.
- **Shipped 2026-09-17:** `beta_changed` ranked in `recache_attribution`
  as `forwarded_beta_rotated` (origin `client`, scope `cache_key`, waste,
  Drift kind) — below drift/head/replay-divergence (the 3 measured turns
  keep their `prefix_head_changed` labels; zero churn), above proxy
  outbound and commit-race/residual (otherwise-clean beta busts used to
  file there). Branch gate untouched; metric label + `docs/observability.md`
  attribution table registered. `markers_changed` deliberately NOT ranked:
  marker layout moves with normal growth (breakpoint stage runs every
  turn), so it would fire constantly — beta holds still for 100+ turns,
  which is what makes a rotation a signal. Remaining: forwarded-model
  route flap still unwitnessed (see Next above).
