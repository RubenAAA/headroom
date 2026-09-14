# Learning: non-PAYG pass-skips are deliberate

- **Source:** `docs/notes/proxy-experiments-2026-08.md` §8 (checked 2026-08-09)
- **Claim:** tool-sort, schema-sort and cache_control auto-placement skip on non-PAYG auth by risk trade (byte mutation looks like cache-evasion → revocation risk), not by defect. Skips never touch `strategies_applied` and report zero tokens either way, so no savings are misattributed. `--auth-mode-policy-enforcement disabled` unlocks all three but coarsely (also changes compression, headers, injection) — an owner's decision.


## Detail

*moved from `docs/notes/proxy-experiments-2026-08.md`*

### 8 — the three inert passes are a deliberate risk trade, and there is a flag

Checked in source 2026-08-09. E1 (tool-array sort), E2 (schema-key sort) and E3
(`cache_control` auto-placement) skip on non-PAYG auth, and a Claude Code
session classifies as `Subscription` by User-Agent even though it carries an
OAuth-shaped token (`auth_mode.rs`).

The gate is **not** a claim that they would not help. Every stated reason is
about how byte mutation *looks* to the upstream — "reordering bytes for a
subscription client can look like cache-evasion and trigger revocation".

Item 8's actual question — is anyone crediting savings to passes that never ran?
— is **no**. The skip path never appends to `strategies_applied`, and even when
these passes do run they report `tokens_before: 0, tokens_after: 0`; they
contribute nothing to `tokens_saved` in either case.

`--auth-mode-policy-enforcement disabled` forces the PAYG pipeline and unlocks
all three (commit `7348ede3`, whose message argues token reduction matters for
subscription users too). It defaults to `enabled`, so they stay off. **The flag
is coarse** — it also changes compression policy, internal-header stripping and
synthetic-header injection — so it trades an account-safety posture for token
savings on every request. That is an owner's decision, not a defect to fix.


## Closure 8

*moved from `docs/notes/proxy-experiments-closures.md`*

**8 — closed 2026-08-12: injection failures are joinable; the other claims are
not current proxy faults.** Across the 469 completed/currently-classifiable
turns used above there are zero live `ctx_inject_row_miss`, get, persist or
search failures. The source-level gap was real: those events had a conversation
hash but no request ID. A controlled missing-row lookup first reproduced an
unjoinable `ctx_inject_row_miss`; after threading the request ID through the
injection decision/build path, the same event contains
`request_id=req-row-miss`. Get, persist and search failures and successful
builds carry the same key. Production injection budgets are now
request-correlated too, so clipped and overrun events join the turn instead of
becoming a second blind spot.

The remaining notes close without code changes. All 21 non-JSON lines in the
current log are three seven-line entries from `restart-headroom.sh`, not
malformed proxy records. The three non-PAYG skip events each occur 469 times, as
configured for subscription auth. There are 60 current volatile-content
warnings but zero under `tools[]`; the varying tool-index sample does not
reproduce, and current warnings carry both request and conversation keys.
