# Idea: isolate the provider noise floor; log which head component moved

- **Status:** SHIPPED 2026-09-17 — floor measured (see Results); head-component
  field landed separately the same day.
- **Source:** every causal argument here assumes same-bytes-equals-hit, but
  Anthropic's commit latency, eviction policy, and cross-node stickiness are
  unknown. Closest prior datum: the 563-event byte-stable window (one method,
  one window). Related gap: on `prefix_head_changed` turns only the head hex is
  logged, so "the client moved model/system/tools" never says which — the
  open question behind the 15,171-token 2026-09-17 event, where forwarded
  tools/system digests looked stable while the client head hash moved.
- **Value:** the irreducible floor every "avoidable?" claim must be net of.
  Without it, residual waste is argued against zero instead of against noise.
- **Next (done):** ~~(a) control experiment~~ measured below; ~~(b) cheap
  field~~ shipped (`head_moved=<component>` on recache lines,
  `a_recache_event_names_which_head_component_moved`).

## Results 2026-09-17: floor is 0/21 at 40s gaps

Method (scripted traffic turned out to be unusable — upstream 429s all
non-CLI-fingerprint subscription requests ~100% while live traffic billed
normally; so the control rode the real CLI): 22 × `claude-work -p` with a
fixed prompt (~40s apart), all wire-identical (msgs/model/beta/markers/
sys/tools digests, prefix+tail ladders, one conversation key).
`~/.headroom-triage/cli_loop.sh`; raw outputs in
`~/.headroom-triage/cli_loop.log`. Turn 1 established footprint 21,498;
turns 2–22 read 21,498 with 0 rewrite, 0 recache events, gaps 40–42s.

**Floor: 0 misses in 21 reads (point estimate 0%).** At 40s gaps with a 21k
prefix the provider never misses. Consequence: the residual bucket's
sub-5s-gap misses are NOT background flakiness — they belong to the
rapid-fire regime (staleness under load), which sharpens both
`recache-residual-triage.md` and `recache-commit-latency-proof.md`.

Caveats: prefix was 21k; the floor may be size-dependent (residual
footprints run to 500k+) — a large-prefix control would test that. Gaps
were ~40s; the floor between 5s and 40s is still unmapped.


## Update 2026-09-17

Part (b) landed the same day (per-component `head_model`/`system`/`tools`
fingerprints; recache lines carry `head_moved=<component>`, covered by
`a_recache_event_names_which_head_component_moved`). The 15k-event anatomy
question is now answerable on the *next* big head event — no log archaeology
needed. Part (a), the control experiment, is still open.

## Blocker: scripted traffic gets per-client upstream 429s (2026-09-17)

The control run (`~/.headroom-triage/noise_floor.py`: fixed 28KB body,
sonnet, 30s gaps, constant 12-beta set) never got past turn 1. Eliminated by
testing, ~40 probes: proxy behavior (direct-to-Anthropic fails identically),
model (opus + sonnet), body (full/minimal/unique), beta set (0/1/12 valid —
and one invalid beta correctly 400s, proving validation itself works),
congestion (fails instantly at 5 lines/sec), cooldown (15 silent minutes
didn't clear it), TLS stack (Python + curl fail alike), streaming flag.
Meanwhile live traffic bills hundreds of turns at ~1–3% refusal — and every
429 cluster that looked like "other stuck sessions" turned out to be my own
requests under other conversation keys. Spend so far: $0 (429s not billed).

Pivot (same day): drive identical turns through the real CLI instead —
`claude-work -p` with a fixed prompt produces wire-identical bodies across
fresh sessions (verified: all fingerprint digests equal, same conversation
key), and separate Anthropic cache entries are keyed by content, not
session, so turn 1 writes and turns 2+ read the same entry. Pilot pair:
turn 1 read=14848/write=6650 (footprint 21498), turn 2 at +40s read=21498/
write=0 — full hit. `~/.headroom-triage/cli_loop.sh` now running 20 turns.
Open incidental finding: scripted (non-CLI-fingerprint) subscription
requests appear to be throttled as a class — worth its own note if it
reproduces, since synthetic monitoring must ride the CLI.

## Negative result: no retrospective control exists (2026-09-17)

Mined 08:04–14:3xZ for consecutive same-conversation turns with identical
forwarded wire state (msgs/model/beta/markers/sys/tools digests,
prefix+tail ladders) 30s+ apart: 37 pairs found, but ALL lack usage records
— they are failed routed turns (`model_route_translate` →
`model_route_fallback` → `upstream_rejected`) being retried identically
minutes later. Failed turns never book usage, so history contains no
byte-identical control group. The live run is the only way to get the number.

- **Update 2026-09-17:** (b) shipped — `PrefixFingerprint` now carries
  `head_model/head_system/head_tools` (same `sample_value` projection as the
  fused head, `usage_observer.rs:prefix_fingerprint_with_model`), compared
  per-stream under the both-known-and-different rule and emitted as
  `head_moved="model|system|tools"` on all four `cache_recache_observed` arms
  plus `vs_stock_turn`. Additive logging only; attribution gating untouched
  (see `recache-absorbed-head-ambiguity.md`). (a) still open: no harness
  measures a provider miss rate today (replay bins skip provider usage,
  simulators have no cache fields, `HEADROOM_CAPTURE_DIR` unset) — control
  must be generated, wiremock-first for determinism, live for the real floor.
