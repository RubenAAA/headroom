# Idea: reasoning-continuity audit (§6)

- **Status:** implemented 2026-09-28 — the audit holds with zero drops over
  5,011 turns; nothing to fix.
- **Source:** `LOOK_AT_THIS_WHEN_YOU_HAVE_TIME.md` §6 + Traps (dropping reasoning cost one model 30% on a coding benchmark plus reconstruction tokens). Proxy shape: `compression/prior_thinking.rs:1-17` drops thinking/redacted_thinking from pre-last assistant turns ONLY on rebuild/history arrivals, replay store carries stripped bytes after; `thinking_drop_is_free` gates tail divergences; last-assistant never touched; Bedrock/Vertex arms drop thinking in translation.
- **Settled by measurement — do not relitigate:** `rejected/prior-thinking-billing-question.md` (2026-09-17: removing 4.5 MB across 67 disciplined turns moved billed reads ~0, 63 exactly 0 — wider dropping saves nothing; current boundary-only behavior correct). Caveat preserved: 480/517 drops co-fire with offload, so the invariance is to the removal bundle. This file proposes ZERO additional dropping; it proves continuity holds (signatures, redacted opaque data, encrypted reasoning items through compress → replay → forward).
- **Implementation constraint:** `rejected/semantic-hold-thinking.md` deferred SSE-path parsing for paint-delay cost. So this audit is offline (captures/replay) plus counters — nothing on the hot SSE path. `rejected/absorbed-rebuild-boundary.md` (rejected as too rare: stabilizers run after the inbound `rebuild_boundary` decision) is the gate this audit protects — cite it, don't duplicate it.
- **Composition note:** thinking is 31–35% of output bytes (`output-bucket-composition.md`) but $0 at read rates per the rejection above — value here is correctness (avoiding full-price plan reconstruction), not savings.
- **Next:** offline trace asserting last-assistant intact, prior stripped only on rebuild boundaries, nothing else dropped; missing-item counter/alert. Audit ships first; any fix is its own revertible commit (doc's "passing back dropped reasoning items" item).
- **Exit:** close when continuity is instrumented with ~zero drops outside rebuilds; future stripping proposals must cite it.
- **Tool:** `crates/headroom-proxy/src/bin/reasoning_continuity_audit.rs` (offline; joins inbound captures with `out/` wire copies by request_id; checks last-assistant intact / no fabricated blocks / monotonic strip per session / signature fidelity. Written 2026-09-24, unverified — tree was mid-refactor; netvalue has full 2,648/2,648 pairing ready).

## Findings 2026-09-28 — continuity holds

`reasoning_continuity_audit` over netvalue (4,528 paired turns, 75 sessions)
and baseline-202609 (483 turns, 3 sessions): last assistant intact on every
turn, 0 fabricated blocks, 0 resurgences, 246,536 kept blocks all signed.

The first run reported 2 last-assistant violations. Both were an empty,
unsigned `thinking` block from a stream cut short, which
`drop_unsigned_reasoning_blocks` (`proxy/reasoning.rs:181`) removes on
purpose because Anthropic refuses it. The audit now skips unsigned blocks
the same way, and reads 0.

No live counter was added. The proxy already logs
`unsigned_reasoning_blocks_dropped` for the only drop outside rebuilds, and
the audit is the check to re-run before any new stripping proposal.
