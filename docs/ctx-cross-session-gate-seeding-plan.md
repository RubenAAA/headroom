# Plan: Cross-session offload-gate seeding (model switches, subagent fan-out)

> **Status: P1 IMPLEMENTED (core mechanism + proof tests green) — wiring (P2),
> integration tests (P3) and rollout (P4) remain. See §6.**
>
> Supersedes the "sibling lanes" sketch from that review, which was wrong
> (see §1): a model switch mints a new *session*, not a sibling lane.

## 1. Problem

Switching models mid-conversation (opus → sonnet, same terminal) — or fanning
out to a subagent on a different model — cold-starts CTX-3 offload. The new
model's turns persist zero blocks (`blocks_offloaded=0, blocks_deferred>0`),
so `headroom ctx search|get` and `headroom_retrieve(hash)` come back empty
for everything from the new lineage, while the old lineage keeps working.

Root cause, verified in code:

- Conversation identity folds the model in:
  `conversation_discriminator = H(model + NUL + canonical(first message))`
  (`crates/headroom-proxy/src/cache_stabilization/drift_detector.rs:1496-1510`),
  and the session key is `auth:<cred-hash>:<conv8>`. **A model switch is a new
  session by design** — "a genuine model switch does start a new provider
  cache lineage" (`derive_session_key_with_model` docs,
  `drift_detector.rs:1398-1407`).
- The `OffloadGate` converted-hash set is keyed by that session key
  (`OffloadPolicy.session_key`, `ctx_offload.rs:581-586`, wired at
  `proxy.rs:4349` and `routed/transforms.rs:204-208`). New session ⇒ empty
  set ⇒ `prior=false` for every frozen block.
- The drift baseline for the new session is also fresh, so
  `rebuild_boundary=false`, and the boundary gate defers every frozen first
  conversion (`ctx_offload.rs:915-921`).
- Retrieval needs no fix: `/ctx/get` is global over the shared `ccr.db` and
  search is per-project. Empty results mean capture never persisted.

What already works and must not regress:

- Same-session new lanes (same model/auth/first-message, rewritten system —
  e.g. agent-type differences in the system prompt) **already share the gate**,
  because the gate never sees lanes (lane = session + system hash,
  `stream_lane_key`, `drift_detector.rs:1028-1033`). Only their drift baseline
  is per-lane. The `stream_lane_detected` subagent fan-out case is covered.
- `OffloadGate::adopt_from` (`ctx_offload.rs:~505`) already seeds a session's
  set from a donor without touching the donor. Used today only by
  prefix-adoption (same history continued, long-history threshold).
- Retrieval (`headroom_retrieve`, `/ctx/*`) is model-independent. A fresh
  subagent conversation handed hashes (e.g. in the Task prompt) redeems them
  on any model with zero proxy changes.

## 2. Cache-safety contract for this change

Extends the contract in `docs/ctx-mode-in-headroom-plan.md` §0 (I1–I6 still
bind in full). Additional invariants specific to seeding:

- **S1 — Seed only at birth, never merge into a live session.** A newborn
  session never emitted bytes, so "convert on first sight" causes no
  mid-stream raw→digest flip. Merging a donor set into a session that already
  forwarded raw bytes would shift its prefix and re-cache upstream. The call
  site MUST install only when the target set is absent. (The existing
  `adopt_from` *extends*; the prefix-adoption path keeps that behavior — its
  adopter continues the donor's bytes, so re-application is stable there. The
  new path needs an absent-only guard.)
- **S1a — Ordering: seed before the session's first gate lookup.** Absence
  alone is NOT the birth signal: `hydrate` installs an *empty* set on miss,
  and a live session that simply never converted anything also holds `{}` —
  seeding it at turn 11 would poison exactly as S1 forbids. Birth is captured
  in the drift first-request branch (`observe_drift → None`), and seeding must
  run before that session's first `gate_lookup` in the same request. The
  ordering holds on both paths today (drift observation precedes offload:
  Claude path `proxy.rs:~3900s` vs `~4400s`; routed path
  `routed/transforms.rs` observe-then-offload) — this plan makes it an
  explicit, tested requirement, not an accident of line order.
- **S1b — Concurrent double birth is benign, do not "fix" it.** Two parallel
  first-requests for the same newborn session may both seed: same donor ⇒
  identical set; different donors ⇒ same-conversation-valid union/last-write.
  No lock dance beyond the single sessions-lock hold in `seed_if_absent`.
- **S2 — Donor untouched.** Clone, never move. The donor lineage's bytes are
  bit-for-bit unaffected.
- **S3 — Drift, replay, and boundary state stay fully qualified.** Session and
  lane derivation, `rebuild_boundary`, replay trackers: zero changes. Only the
  converted-hash set is shared. Seeded conversions ride the existing `prior`
  path, which needs no boundary.
- **S4 — Same credential only.** Donor and recipient must share the auth-hash
  component. (No information flow exists even in theory — a hash matches only
  content the recipient already holds byte-identically — but cross-credential
  sharing is needless attack surface. Keep the rule.)
- **S5 — Conversions stay content-deterministic.** Seeding changes *which*
  sessions convert on first sight, never *what bytes* a conversion emits
  (digest = f(block bytes, budget), I1). A seeded session emits byte-identical
  output to what the donor emitted for that content.

Non-goal, stated plainly: cross-model cache *hits* are impossible (cold
lineage per model regardless of proxy behavior). The win is **context bytes,
not hits** — digests from the new lineage's first request instead of full raw
history until a boundary fires.

## 3. Design

### 3.1 Donor matching: model-free secondary index

Session keys are opaque (`H(model, first-msg)` is not invertible), so "same
conversation, other model" needs its own index:

```
(model_free_key) -> bounded list of session keys, most-recent-first
model_free_key = (auth_hash_component, H(canonical(first message)))
```

- `H(canonical(first message))` reuses the existing extraction
  (`conversation_messages(body, kind).first()`) and canonical writer
  (`write_canonical`) already used by `conversation_discriminator` — same
  bytes, minus the model prefix.
- Auth component reuses the `auth:<h16>` / opener part already parsed at
  session derivation; S4 falls out of the key. Explicit-`x-headroom-session-id`
  sessions return `None` (operator-managed identity already shares everything;
  nothing to seed).
- Update **only on the rare first-request path** (new session observed) — no
  hot-path lock. Bound each entry (cap ~4 session keys, drop oldest; stale
  models age out naturally). Memory: a few dozen bytes per conversation.
- Lookup at newborn-session first request: iterate birth-ordered candidates,
  take the first with a non-empty set. ("Most recent" means most recently
  *born*, not most recently *active* — birth-only updates cannot track
  activity, and it does not matter: hashes do not expire semantically,
  persisted files are age-gated by `GATE_PERSIST_MAX_AGE`, memory by LRU.)
- Cap the installed seed (`SEED_MAX_HASHES`, generous — live traffic shows
  ~26 conversions per 51 turns): per-session sets are unbounded, so a
  pathological donor must not clone unbounded into every newborn session.
- Cloning is verbatim, so the `p512:` budget namespace rides along: a block
  the donor converted under the legacy budget re-emits the legacy digest —
  required, since that digest is already in the provider's cache.
- A newborn session's `put` may LRU-evict its own donor from memory; harmless
  (donor rehydrates from its persisted file on next use).

Donor/recipient matrix (all verified against current keying):

| Case | Session | Lane | Gate today | After seeding |
|---|---|---|---|---|
| Model switch, same history | new (model in hash) | new | empty → stall | seeded from donor |
| Subagent, same model+first-msg, new system | same | new (system) | **shared already** | no change |
| Subagent, fresh Task prompt | new (first-msg) | new | empty, no donor match | warms normally (correct: nothing known) |
| Resumed/forked same conversation | new or adopted | new | empty unless prefix-adopt fires | seeded from donor |
| Unrelated conversation | new | new | empty, no match | unchanged |

### 3.2 Seeding call

At newborn-session first request (the `cache_drift_first_request` branch —
confirmed present on both paths: Claude path observes drift per lane before
offload; routed path `observe_drift → None ⇒ rebuild_boundary=false` in
`routed/transforms.rs`), when the gate has no set for the new key:

1. Look up `model_free_index[(auth, firstmsg)]` → donor session key(s) ≠ new.
2. `gate.seed_if_absent(donor_session, new_session)` — new `OffloadGate`
   method: clone donor set (in-memory `peek`, else persisted digest file —
   same two sources as `adopt_from`), install **only if target absent, under
   the single sessions-lock hold** (the S1b race analysis), persist the
   installed set (mirroring `adopt_from`'s persist tail).
3. Emit `offload_gate_session_seeded` (donor session-hash prefix, conversions
   count) next to the existing `offload_gate_adopted` / `stream_lane_detected`
   lines.

Plumbing note: do NOT change `derive_session_key*` signatures (hot function).
Add a small `pub(crate)` helper in `drift_detector.rs` reusing
`conversation_messages` + `write_canonical`, called only on the rare branch.

No changes at `proxy.rs:4349` / `routed/transforms.rs:204-208` (policy wiring
is key-agnostic), no changes to drift/replay/observer, no changes to
retrieval paths.

### 3.3 Flag

New opt-in flag following the stabilizer convention (off by default):
`--ctx-offload-cross-session-seed` (config plumbing per `config.rs` CTX-3
block, surfaced in `docs/flags.md`, i.e. `contrib/headroom-flags.sh`).
Rationale for off-by-default: seeding changes wire bytes for newborn
sessions (raw → digest from request 1); that is the intended win, but it is
still a behavior change and must be rolled out deliberately.

## 4. Implementation phases

- **P1 — Index + seed primitive (headroom-proxy, unit-tested).** ✅ DONE
  (mechanism + proof tests green; see §6). `model_free_lineage_key` helper,
  bounded birth-order index, `OffloadGate::seed_if_absent` with absent-only
  guard under a single lock hold. Proof tests: sight-conversion on a seeded
  newborn session vs Deferred unseeded twin, live-refusal, donor immutability,
  cross-turn byte-stability, hydrate-ordering, empty-donor no-op.
- **P2 — Wire both request paths + observability.** ✅ DONE as `d3aa536d`.
  Birth signal is a new `observe_drift_with_birth` returning
  `(dims, first_sight)`, NOT `dims.is_none()` (stable append-only turns also
  return `None` — verified in `observe()` arms; inferring birth from dims
  would seed live-never-converted sessions mid-history, the S1a poison case).
  Both paths call it at the existing drift site (Claude: `proxy.rs` lane
  block; routed: `routed/transforms.rs`), then a shared
  `ctx_offload::seed_newborn_session` helper (note birth → try candidates →
  event + counters) gated on the new `--ctx-offload-cross-session-seed`
  flag (off by default, plumbed `CliArgs → Config → CtxOffloadConfig`).
  `offload_gate_session_seeded` event; counters `offload_gate_seeded_total`
  / `offload_gate_seed_refused_live_total` (`metric_names.rs`,
  `ctx_metrics.rs`, with getter + self-test coverage).
  Verified 2174/0 on the exact committed content (isolated worktree —
  main-tree verification was blocked by another session's in-progress
  `openai_buffered_ccr.rs`, which did not compile at the time).
- **P3 — Integration tests.** ✅ DONE as `77eb1a4d`.
  `tests/ctx_cross_session_seed.rs` drives the wired order with real
  components (6 tests, all green; full proxy lib 2188/0): model-switch
  first-sight conversion with donor bytes, live-session non-seeding,
  cross-credential isolation, intra-session sharing without seeding,
  persistence round trip, I5-style byte-stability golden across a switch.
  The persistence test caught a real gap: the lineage index was
  memory-only, so post-restart seeding found no donors — the index now
  persists to `lineages.json` (same age rule, lock-nesting-free writes).
- **P4 — Docs & rollout.** ✅ DONE (`bb695832` docs + canary live).
  `docs/flags.md` regenerated (diff exactly the new entry), commented
  flag-file entry with canary pointer, CHANGELOG under Unreleased. Canary:
  release binary with P1–P3 deployed via `restart-headroom.sh`
  (clean restart, in-flight drained, health OK) with
  `HEADROOM_PROXY_CTX_OFFLOAD_CROSS_SESSION_SEED=true` in the process env
  (verified) — ephemeral by design, gone on next restart. Trigger is a real
  model switch; watch `offload_gate_session_seeded` +
  `ctx_offload_accounting{blocks_offloaded>0}` on the first turn after it.
  Make permanent by uncommenting the flag-file line; revert with a plain
  restart (previous binary kept at `~/.local/bin/headroom-proxy.prev`).

## 5. Risks / open items

- **First-message stability.** Seeding keys on the first message; if a client
  rewrites message 1 on model switch (not just the `model` field), no donor
  matches and behavior is today's (safe fallback, no error). Worth one
  canary check with real Claude Code `/model` switches.
- **`hydrate` IO on the lookup path.** Donor peek may read a persisted gate
  file; acceptable — this fires once per newborn session, same rarity class
  as the existing `offload_gate_rehydrated` path.
- **Index growth.** Bounded per entry (§3.1) plus the existing
  `GATE_PERSIST_MAX_AGE` sweep semantics for the persisted side; add a
  canary watch on process RSS before enabling fleet-wide.
- **Routed-path identity parity.** The routed path derives with
  `identity_model`; confirm the model-free component matches the Claude path
  for the same conversation (P3 test with a rerouted fixture), else donors
  silently miss across paths.
- **Not covered, deliberately:** fresh subagent conversations with novel
  prompts (nothing to seed from — retrieval-by-hash already serves them);
  merging lanes/sessions after first emission (forbidden by S1).

## 6. Proof (P1) — mechanism verified against the real code, no mocks

Committed as `62989e90` (rewritten once after a parallel session's worktree
reset wiped the uncommitted first copy — see note below; the committed copy
re-ran green before commit).

Implemented: `model_free_lineage_key` + shared `identity_branch_id`
(`drift_detector.rs`, with `derive_session_key_with_model` refactored onto
the branch helper — all pre-existing session-key tests green, so the hot-path
strings are byte-identical); `OffloadGate::{note_session_birth,
seed_candidates, seed_if_absent}` + `SeedOutcome` + caps
(`ctx_offload.rs`).

Results (`cargo test -p headroom-proxy --lib`, real
`offload_anthropic_request` + real `OffloadGate`):

- Seeded newborn session converts a donor-known frozen block on first sight
  with `rebuild_boundary=false` (`blocks_offloaded=1, deferred=0`); the
  unseeded twin on identical input defers (`0/1`, prefix bytes unchanged) —
  the seed, and only the seed, removes the stall.
- Seeded digest bytes are byte-identical to the donor's, and re-application
  on turn 2 emits identical bytes (I2 holds; no revert-to-raw).
- Live session: `RefusedLive`, set keeps exactly its own conversions
  (disjoint from donor's) — late merge refused as S1 requires.
- Hydrate-installed empty set refuses (S1a backstop pinned); empty donor
  installs nothing; pathological 4106-hash donor truncates to
  `SEED_MAX_HASHES`; donor set bit-identical before/after.
- Lineage keys match across models, differ across opener/credential, `None`
  on explicit sid / messageless bodies, 64-hex message half.
- Known contract limit, pinned by test: the callee proceeds for
  absent-but-live sessions (live-never-converted holds no set), so birth
  knowledge MUST come from the caller's drift first-sight signal (S1a
  ordering) — wiring (P2) must preserve drift-before-offload order.
