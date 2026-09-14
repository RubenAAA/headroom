# Learning: early recache hypotheses refuted (H1–H3)

- **Source:** `docs/notes/recache-classification.md` hypothesis ledger (proxy PID 56134, 2026-08-22: 502 requests, 27 events; whole loss 14,207 tokens)
- **Claim:** the first three hypotheses died on measurement; H4 stayed the lead (see `recache-h4-unread-blocks.md`).


## Context

*moved from `docs/notes/recache-classification.md`*

# Hypothesis ledger — 2026-08-22

The July analysis above named three causes without measuring how much each
accounts for. This section is a running ledger instead: one entry per
hypothesis, each carrying its status and the evidence that put it there. A
refuted entry stays in the file. The point is that nobody tests it twice.

**Measurement window.** Proxy PID 56134, started 13:21 local on 2026-08-22
(09:21 UTC — the log timestamps in UTC and the process start prints local,
which is worth knowing before writing any filter over it). 502 requests, 27
recache events, two conversations involved. `restart-headroom.sh` does not
roll `~/headroom-proxy.log`, so anything read from that file without a
timestamp filter mixes in older builds.

**The whole loss, this window: 14,207 tokens across 20 turns.** Small. Worth
sizing before anyone spends a week on it.

| Attribution | Events |
|---|---|
| `unexplained_after_replay` | 20 |
| `prefix_content_diverged` | 4 |
| `aftershock_of_diverged_prefix` | 2 |
| `early_messages` | 1 |


## Detail

*moved from `docs/notes/recache-classification.md`*

## H1 — The proxy moves the cache hot zone. REFUTED

The reason `outbound_drift_state` and `observe_outbound_drift` were built:
the inbound hash is taken before any stage runs, so proxy-caused movement in
`system`, `tools` or the first three messages could not appear in
`drift_dims`.

It is not happening. Across 502 requests the outbound detector logged 8
first-request events and 2 drift events, and both drift events paired 1:1
with an inbound drift on the same session about 60ms earlier — the client
moved, and we carried it. The `origin: "proxy"` branch has never fired.

The instrument works; the answer is no. Keep it — it is what lets the next
person skip this hypothesis in one query.

## H2 — Volatile content (UUIDs, timestamps) deep in history. REFUTED as the main cause

The July table blamed "UUIDs, timestamps in messages[3+]" for 20+ unknown
events. Measured: 3 of 27 recached turns carried any volatile finding, against
a base rate of 15/502 (3%) across all requests. Enriched roughly fourfold, so
the effect is real, but it is six findings and cannot account for twenty
events.

Locations on recached turns ran `messages[16]` to `messages[107]` — all past
the hot zone, so widening the drift hash to cover them would explain three
events and no more.

## H3 — Subagent close or `/clear` resets the conversation. REFUTED for these events

The July hypothesis. It does not fit this window. All 20 events land
mid-conversation with message counts growing monotonically (3, 15, 19, 40,
48, 59, 67, 73, 81, 91, 97, 111, 113, 121, 123) and an active prefix-replay
chain reaching `chain_id` 25. A context reset would restart that chain, not
deepen it.

It may still explain events in other windows. It does not explain these.
