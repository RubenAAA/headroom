# Idea: whole-tree cost attribution (§6)

- **Status:** open — the tool now joins Claude subagents to their parent;
  routed Codex/Spark workers join only in captures taken after 2026-09-28.
  Re-arm capture (`HEADROOM_CAPTURE_DIR`) at the next restart that happens
  anyway, not a restart made for it.
- **Source:** `LOOK_AT_THIS_WHEN_YOU_HAVE_TIME.md` §6 + §8 (per task, not per request; workers ≥69%, 90%+ typical; planner choice swung worker spend several-fold). Proxy routes and paces fan-out but prices turns.
- **Routing design lives elsewhere:** `intelligent-task-aware-routing.md` (open) already owns the NewAsk-pin design and cites the same rejection below — this file is its measurement prerequisite, not a second design. `rejected/first-turn-write-sharing.md` (D4 dead: ~8.1k savable of 6.3M first-turn writes over six days, 0.13%) and `rejected/recache-rekey-floor.md` (hidden re-key floor ~0.5M over ten days; the earlier 3.37M was shared-prefix reads) set the floor this join must include.
- **Not a retry of:** `rejected/per-turn-model-routing.md` (moving one 134k opus turn: $0.54 rewrite vs ~$0.08 read+output — loss unless the whole conversation moves). Tree cost is the instrument that catches that. `rejected/fanout-density-unearned-writes.md` (concurrency 6.3% of unearned over six days; depth outweighs 10×) — no wider gate without a shared object shown.
- **Join constraints (each violated into a wrong finding before):**
  - `conversation-key-merges-streams.md` (79% events on merged keys): respect lanes/alternates (`usage_observer.rs:40-46`), or cross-lineage noise returns.
  - `recache-counting-rules.md` + `first-turn-write-share.md`: separate first turns (42% of writes; 19.6% of creation) or comparisons invert.
  - `rejected/recache-rekey-floor.md`: `fresh_session` contradictions are mostly shared-prefix reads, not re-keys; do not count them as waste.
  - `ccr-fragmentation-across-workers.md`: per-worker state fragments under round-robin — tree join must be worker-aware.
  - `sidecar-poisons-main-cache.md` (fixed via strip-long-context-beta): label/exclude sidecar turns or one spinner answer poisons the tree.
  - `rejected/ccr-round-cap-forced-answer.md` (rejected: retrieval loops ran ~220 calls vs 30–70 until 2026-09-25, none since): attribute retrieval rounds to the tree or loops hide.
- **Next:** lane-respecting planner+worker trees via identity/ctx-capture; cost/tree by billing type, turns/tree, hit rate. Gate routing proposals on it: ship only when tree cost drops with guardrails flat.
- **Exit:** close when tree cost is queryable; later routing ideas cite it instead of per-request math.
- **Tool:** `crates/headroom-proxy/src/bin/whole_tree_cost.rs` (offline; groups captures by envelope session key across models, planner = first-turn model as stated assumption, per-model shares per tree. Written 2026-09-24, unverified — tree was mid-refactor; run on netvalue/blindguard once green. Re-keyed continuations file as separate trees — ledger `session_key_hash` join remains the follow-up).

## Findings 2026-09-28 — trees were one model because the key was wrong

Run as written, `whole_tree_cost` found 75 trees in netvalue, every one a
single model, worker share 0%. The envelope `session_key` is a hash of the
opening prompt, and each subagent has its own prompt, so each worker filed
as its own tree. It also skipped all 3,861 routed turns (Codex, Spark).

Claude Code's `metadata.user_id` holds a `session_id` that subagents share
with their parent, so the tool now groups on that and reads Responses bodies
too. Rerun over netvalue: 52 trees; one links opus with sonnet workers
(1,145 turns, sonnet 20% of the cost). Routed turns still cannot join:
their captured body is the translated one, which has no `metadata`, so
they pile into one tree per credential.

The fix for that is in `capture.rs`: the envelope now carries
`client_session_id`, read from the client's own body on both the forward
and routed paths. It changes nothing on the wire and only runs when
capture is on. The first capture after a restart will show whether
Spark/Codex worker cost belongs in routing decisions.
