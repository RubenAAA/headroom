# Idea: size the hidden re-key / first-turn floor

- **Status:** open (pure log query; no code)
- **Source:** a continuation under a fresh key (compaction, model switch,
  system rewrite) files as `FirstTurn`, never reaches attribution, and its
  write is not waste-counted. `first_turn_contradictions_total` and
  `forgotten_conversations_total` bound this from below but nobody has sized
  it against recache waste. The 2026-09-17 completion log now parks the
  session hash beside every key, which is the missing join key.
- **Value:** the recache waste figure has an unmeasured floor. If large, the
  fix is key stability, not cache stability — a different project.
- **Next:** join `first_turn_write_observed` /
  `first_turn_prefix_diagnostic` to recently-completed sibling keys on
  `session_key_hash`; total `arrived_with_history`-with-no-read plus forgotten
  returns over the same window as recache waste. Small ⇒ close this; material
  ⇒ promote to a key-stability proposal.
- **Update 2026-09-17:** measured over the live 08:04–13:24Z window —
  drift waste 69,650t (8 events) vs 14 contradictions at 223,927t (3.2×),
  arrived-with-history-no-read 0t on thin D0 coverage; 60s session-hash join
  found zero siblings for all 14 (join works — 3 multi-key sessions linked —
  but blind to session-rotating rekeys by design). Join gaps closed in code:
  `turn_cost_ledger` now carries `session_key_hash`,
  `cache_conversation_forgotten` logs `evicted_footprint_tokens` + current
  write/read, `cache_stream_unmatched` carries `session_key_hash`
  (`usage_observer.rs`). Still open: re-run the join once new fields land,
  then promote-or-close on the benign-scaffold share.
