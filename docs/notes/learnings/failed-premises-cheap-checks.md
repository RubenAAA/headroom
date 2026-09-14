# Learning: three failed premises and the discipline they earned

- **Source:** `docs/notes/proxy-experiments-2026-08.md` §25
- **Claim:** each looked supported; each died to a cheap available check. Rule: before attributing tokens, list every other process with the same signature and rule them out by query; scope by date (unrotated log + 8 rebuilds mix schemas).


## Detail

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 25 — Premises that failed on 2026-08-09, and what killed each

Kept because each one looked well-supported when acted on, and the thing that
disproved it was cheap and available beforehand. All three share a shape:
**a mechanism was confirmed to exist, then a large number was attributed to it
without checking what else produces the same signature.**

### 1. "Replaying the leading run that still agrees recovers busted prefixes"

*Predicted:* a divergence at message k throws away k messages of cached prefix,
so replaying `prev_fwd[..k]` recovers them.

*Killed by:* the same conversation's own turns. A `no_previous_turn` turn, which
replayed nothing at all, still read 222,975 tokens from cache. Compression is
deterministic, so a turn's own bytes for an unchanged prefix already reproduce
what the provider holds — a decline was never the loss. The partial splice
matched neither turn's bytes and cost 204,768 tokens on its first firing.

*Available beforehand:* `no_previous_turn` turns with full cache reads were
already in the log. The skip reason was read as a cost without ever checking
what those turns billed.

### 2. "`<system-reminder>` churn costs 19% of the input bill"

*Predicted:* the client withdraws a reminder from an early `tool_result`
message, breaking the prefix there.

*Killed by:* splitting the same waste by whether the conversation key carries
more than one advancing sequence. 60% of it sat on multi-sequence keys, where a
divergence at message 8 means "one sequence has a reminder there, the other does
not" rather than "the client withdrew it". The defensible figure is **1.5%**.

*Available beforehand:* message counts running backwards is item 11's documented
fingerprint and had been in this document all day.

### 3. "`conversation_key` merges concurrent sessions"

*Predicted:* three Claude Code sessions on one machine share auth and IP, so
`derive_session_key` collapses them and `conversation_key` is left to separate
everything on `system + messages[0]`.

*Killed by:* counting them. **68 distinct `session_key_hash` values in one day,
each mapping to exactly one conversation key.** Sessions and subagents are
already separated; nothing is collapsing.

*Available beforehand:* one `group by` over a field already on every
`cache_recache_observed` event.

### What survives

- Replay hits 328 of 344 full replays cleanly.
- Cache writes are 55% of the input bill; compression removes 3.1% of wire bytes
  and is worth ~1.5% at most.
- 84% of re-cache waste sits on keys whose turns need more than one advancing
  sequence. **The observation survives; the cause does not.** Compaction,
  branching and concurrency all produce that shape and have not been separated.

### The measurement discipline this earns

Before attributing tokens to a cause, list every other process that produces the
same signature and rule them out by query. Message counts, skip reasons and
block shapes are all consistent with several causes at once. And scope by date:
`proxy.log` is never rotated, and the proxy was rebuilt eight times today, so
"today" mixes log schemas as well as months of history.
