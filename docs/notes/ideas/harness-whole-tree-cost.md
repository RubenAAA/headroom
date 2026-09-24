# Idea: whole-tree cost attribution (§6)

- **Status:** open — guardrail instrument, no request-path change, no routing design here
- **Source:** `LOOK_AT_THIS_WHEN_YOU_HAVE_TIME.md` §6 + §8 (per task, not per request; workers ≥69%, 90%+ typical; planner choice swung worker spend several-fold). Proxy routes and paces fan-out but prices turns.
- **Routing design lives elsewhere:** `intelligent-task-aware-routing.md` (open) already owns the NewAsk-pin design and cites the same rejection below — this file is its measurement prerequisite, not a second design. `first-turn-write-sharing.md` (D4 dead: ~8.1k savable of 6.3M first-turn writes over six days, 0.13%) and `recache-rekey-floor.md` (3.37M hidden floor, 6.7× drift waste) set the floor this join must include.
- **Not a retry of:** `rejected/per-turn-model-routing.md` (moving one 134k opus turn: $0.54 rewrite vs ~$0.08 read+output — loss unless the whole conversation moves). Tree cost is the instrument that catches that. `rejected/fanout-density-unearned-writes.md` (concurrency 6.3% of unearned over six days; depth outweighs 10×) — no wider gate without a shared object shown.
- **Join constraints (each violated into a wrong finding before):**
  - `conversation-key-merges-streams.md` (79% events on merged keys): respect lanes/alternates (`usage_observer.rs:40-46`), or cross-lineage noise returns.
  - `recache-counting-rules.md` + `first-turn-write-share.md`: separate first turns (42% of writes; 19.6% of creation) or comparisons invert.
  - `recache-rekey-floor.md`: include re-key/arrived-with-history floor or trees undercount.
  - `ccr-fragmentation-across-workers.md`: per-worker state fragments under round-robin — tree join must be worker-aware.
  - `sidecar-poisons-main-cache.md` (fixed via strip-long-context-beta): label/exclude sidecar turns or one spinner answer poisons the tree.
  - `ccr-round-cap-forced-answer.md` (open: retrieval loops run ~220 calls vs 30–70): attribute retrieval rounds to the tree or loops hide.
- **Next:** lane-respecting planner+worker trees via identity/ctx-capture; cost/tree by billing type, turns/tree, hit rate. Gate routing proposals on it: ship only when tree cost drops with guardrails flat.
- **Exit:** close when tree cost is queryable; later routing ideas cite it instead of per-request math.
- **Tool:** `crates/headroom-proxy/src/bin/whole_tree_cost.rs` (offline; groups captures by envelope session key across models, planner = first-turn model as stated assumption, per-model shares per tree. Written 2026-09-24, unverified — tree was mid-refactor; run on netvalue/blindguard once green. Re-keyed continuations file as separate trees — ledger `session_key_hash` join remains the follow-up).
