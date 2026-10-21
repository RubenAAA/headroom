# Upstream triage: `976aa714` → `4c7acba5` (63 commits)

Merged as `7398b5c2`. Fetched 2026-10-21. Classes as in
[`upstream-triage-3c418f6f-976aa714.md`](upstream-triage-3c418f6f-976aa714.md):
PORT, ALREADY-THERE, N/A. Python paths map to `upstream-python/headroom/`.

Totals: **13 PORT (11 landed, 2 deferred), 13 ALREADY-THERE, 37 N/A**.

## Landed

| sha | commit | what landed |
|---|---|---|
| `2016dc6d` | `7398b5c2` (merge) | magika ORT session with one intra-op thread; upstream's `magika_idle_threads` test |
| `132dbc76`, `49751ec4`, `683c9a61` | `7398b5c2` (merge) | rustls 0.23.46, quinn-proto 0.11.19, the cargo-minor-patch group (tree-sitter stays pinned) |
| `486afaac` | `c58c19ae` | reject `64:ff9b:1::/48`; the well-known NAT64 unwrap was already here |
| `2cbdd22a` | `cbae05bf` | batch store: replacement takes no slot; expired entries go before live ones |
| `ba824539` | `25af4e1c` | `read_flags_inert` startup warning, in every mode (both flags are unwired here) |
| `97680e77` | `23ff0651` | `savings_ledger_append_failed`, warn once per path, re-armed on success |
| `30b22df7` | `edd2fe3b` | `x-session-affinity` for api.fireworks.ai, value a hash of the session key |
| `46306024` | `6e3c581a` | Responses `cache_write_tokens` booked on HTTP, SSE, Codex WS, routed, capture and hook paths |
| `85520c29` | `6e3c581a` | tools a turn hook adds come off the saving |

Also from this pass, not upstream: `cargo deny` found RUSTSEC-2026-0367..0369
(zip), -0382 (rustls-webpki), -0383 (webpki-root-certs); cleared in
`234896ae` (zip 9, calamine 0.37).

## Deferred

- `f380692b` pinned embedder snapshot: the Rust half is upstream #4190, not
  on `upstream/main` yet. `relevance/embedding.rs` loads `BGESmallENV15`
  unpinned.
- `fbf7bc32` turn hooks on Responses requests: nothing in Rust registers a
  hook that would use it.

Not ported, worth a look: `1cf4d053d`'s per-request response recording (the
tail-edit match already avoids the bust it fixed), and the Anthropic batch
continuation lacking `headroom_retrieve` (the same bug `87574588` fixed for
Gemini). `openai/stream.rs` still ignores `cache_write_tokens` in its
Anthropic-shape translation; it has uncommitted local edits, so it was left.

## ALREADY-THERE

`30b22df7` (message_delta usage half), `8d468d45` base64 0.23, `df11f274`,
`00093ced`, `8cedf7be`, `62b3bedab`, `762a337b`, `290f72c5`, `1cf4d053`
(mostly), `2721f060`, `9762a14a`, `ca6ba348`, `a9eb2a7b` (Rust never hoists
system messages).

## N/A

Python-only logging, refactors, Copilot/wrap/learn/telemetry/kompress-download
features with no Rust counterpart, docs, CI, and pip/npm bumps.
