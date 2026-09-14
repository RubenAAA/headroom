# Idea: remove the redundant outcome-context re-parse

- **Status:** closed 2026-09-11, not doing (09-11 re-audit)
- **Source:** `docs/notes/proxy-followups.md` §5
- **Summary:** phase timers on a 743 KB body put the outcome-context stage at
  7.5 ms; its "cheap, happens once" comment is wrong twice — not cheap at that
  size, and the fifth parse of the body. It can reuse an existing parse. Price
  the rest of parse-once at 3.9 ms, not 54 (the 54 was the savings tracker,
  since fixed). Tool-schema-compaction memoisation already exists
  (`tool_schema_compaction.rs` cache); the 0.6 ms left is the hit path.
> **09-11 outcome:** superseded — routed extraction threads parsed values; no fifth parse exists.
- **Next (superseded):** re-measure on current source (August line numbers stale), then reuse
  an existing parse. See also `parse-body-once-measurement.md` — the fuller version.


## Detail

*moved from `docs/notes/proxy-followups.md`*

## 5. The outcome-context re-parse is still redundant

**Closed 2026-09-11 as superseded — nothing to do.** `build_routed_outcome_context`
(`routed/outcome.rs:19`) takes an already-parsed `&Value`; there is no
`from_slice`/`from_str` anywhere in `outcome.rs`, and `num_messages` comes
from a `.get("messages")` on the threaded parse. The routed extraction fixed
this structurally by threading `parsed` through — the fifth parse no longer
exists. The 7.5 ms figure below is August history.

**Status: open, last measured 2026-08-19 and not confirmed since.** Phase timers
on a 743 KB body put the outcome-context stage at 7.5 ms. Its comment at
`proxy.rs:3510` — "Re-parses `buffered` for model/num_messages (cheap, happens
once)" — is wrong twice: it is not cheap at that size, and it is the fifth parse
of the body. It can reuse an existing parse. Check the current source before
costing the work; the line number is from August.

Price the rest of the parse-once idea at 3.9 ms, not 54.
`maybe_compact_tool_schemas` (`proxy.rs:1781`), `maybe_stabilize_tool_order` and
the tail-breakpoint stage each parse the whole body, edit it and re-serialise,
and those tool stages account for 3.9 ms between them. The 54 ms once attributed
to them was the savings tracker, since fixed in
`crates/headroom-core/src/savings_tracker.rs`. Memoising tool-schema compaction
on a hash of the `tools` array, which used to be listed here as the second fix,
already exists as `cache_key`/`cache_get`/`cache_put` in
`tool_schema_compaction.rs:367`. The 0.6 ms left there is the cache *hit* path
building the key and cloning the value out of the mutex, and it recomputes
nothing.
