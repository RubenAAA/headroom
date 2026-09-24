# Idea: 429s book failed work as well as the rate-limited counter

- **Status:** REJECTED 2026-09-24. A 429 reached the failure bucket, not
  the success funnel — tokens were spent for zero usable output, so the
  ledger must see the waste. The ledger's job is token truth, not fault
  attribution; matching upstream (`record_rate_limited` only) would make
  provider throttling invisible in `net_tokens_saved`. Both call sites
  (`ProxyOutcomeSink::record_rate_limited` in `proxy.rs`,
  `CodexWsOutcomeSink::record_rate_limited` in `websocket_codex.rs`) keep
  the `record_failed` line deliberately.
- **Source:** port of upstream `85fac8c3` (stats funnel, Sept 2026 session).
  Upstream routes 429 to `record_rate_limited` *only*. Our `record_failed`
  is what feeds `record_failed_work`/the savings ledger
  (`proxy.rs:record_rate_limited` calls `self.record_failed(outcome)`),
  so a pure port would make 429s vanish from failed-work accounting
  entirely. The port kept both: counter increment + ledger booking.
- **Value:** correctness of failed-work accounting; the alternative is
  429s invisible in the ledger. No byte/token change either way.
- **Next:** decide whether 429s belong in the ledger at all. If not,
  drop the `self.record_failed(outcome)` line in both
  `ProxyOutcomeSink::record_rate_limited` (`proxy.rs`) and
  `CodexWsOutcomeSink::record_rate_limited` (`websocket_codex.rs`).
  Either way, close this file with the decision recorded.
