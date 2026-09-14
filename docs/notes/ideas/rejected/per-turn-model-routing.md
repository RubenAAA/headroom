# Rejected: per-turn model routing to a cheaper model

- **Status:** rejected 2026-09-03 (the math loses)
- **Source:** `docs/notes/savings-ideas-1.md` (ruled out)
- **Summary:** moving one 134k-context opus turn to sonnet costs a 134k sonnet
  1h write ($0.54) against ~$0.08 for the opus cache read + output. A loss
  unless the whole conversation moves — which is the client's choice, not the
  proxy's.


## Detail

*moved from `docs/notes/savings-ideas-1.md`*

- **Per-turn model routing** (`model_router.rs`, `--extra-model-route`): moving
  one opus turn at 134k context to sonnet costs a 134k sonnet 1h write ($0.54)
  against about $0.08 for the opus cache read plus output. A loss unless the
  whole conversation moves, which is the client's choice.
