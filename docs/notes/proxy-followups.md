# Proxy follow-ups

> Note (2026-09-10): `proxy.rs`/`ccr_stream.rs` line numbers below are
> August-era — current anchors: upstream client build at `proxy.rs:323`
> (keepalive + 90s pool at `:332-336`), `tool_schema_compaction`
> cache at `tool_schema_compaction.rs:207/250/258` (call `:367-383`).
> Statuses ("fixed"/"open") were re-verified 2026-09-10 and still hold.
>
> Note (2026-09-11): full re-verification against the tree at `2914e9ac`
> + worktree and `~/headroom-proxy.log` 2026-09-10 13:06–22:55 UTC (~10 h,
> 2,586 streams). Items 1–4 closed, item 5 superseded, item 6 closed,
> item-3 residue dissolved (details at each entry). Nothing below is
> actionable except the new dead-code note under item 3 — and that one
> says hands off.

Items 1 to 4 were investigated 2026-08-23 against `~/headroom-proxy.log`
(~3,200 streams, proxy restarted 16:47). Items 5 and 6 come from the offload-gap
round of 2026-08-18 to 08-21. Each entry records what the evidence says, not
what it was assumed to say.

> **Moved to [`ideas/implemented/volatile-warning-dedup.md`](ideas/implemented/volatile-warning-dedup.md)** — full item incl. 09-11 closed confirmation.

> **Moved to [`ideas/implemented/ccr-log-level-fix.md`](ideas/implemented/ccr-log-level-fix.md)** — full item incl. 09-11 confirmation.

> **Moved to [`ideas/rejected/proxy-dead-code-decisions.md`](ideas/rejected/proxy-dead-code-decisions.md)** — full item incl. 09-11 residue dissolution + new dead-code note.

> **Moved to [`ideas/implemented/tcp-keepalive-fix.md`](ideas/implemented/tcp-keepalive-fix.md)** — full item incl. 09-11 closure (environmental resets, ~1 abort/10 h).

> **Moved to [`ideas/rejected/outcome-reparse-redundancy.md`](ideas/rejected/outcome-reparse-redundancy.md)** — full item incl. 09-11 supersede (no fifth parse since routed extraction).

> **Moved to [`ideas/implemented/memory-launcher-env-check.md`](ideas/implemented/memory-launcher-env-check.md)** — launcher-env half incl. 09-11 half-close (launcher warns; habit retained).

> **Moved to [`ideas/implemented/threshold-tests-absolute-scores.md`](ideas/implemented/threshold-tests-absolute-scores.md)** — threshold-test half incl. 09-11 closed (pins exist).

