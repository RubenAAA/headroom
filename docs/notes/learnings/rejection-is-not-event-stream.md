# Learning: a rejection is not an event stream

- **Source:** `docs/notes/proxy-experiments-2026-08.md` §9 (answered 2026-08-09)
- **Claim:** requests with no completion record were Anthropic 400s — a
  rejection produces no SSE, so the parser never runs and nothing downstream
  fires. No dropped-`JoinHandle` panic theory needed (though the waiter that
  logs panics/cancellations was still added — structure, not diagnosis).
- **Rule:** reconcile dispatches against `upstream_rejected` before inventing
  blind spots.
