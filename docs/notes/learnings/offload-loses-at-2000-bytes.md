# Learning: offload loses at a 2,000-byte floor; repeat retrievals are why

- **Source:** `scripts/offload-ledger.py`, run 2026-09-24 over proxy logs from
  2026-09-17 08:04Z (7.4 days), `ccr.db`, and Claude Code transcripts.
- **Claim:** on the Anthropic path, the retrievals and re-reads offload causes
  cost 126% of what it saves under API weights (119% under subscription). Two
  thirds of retrieval calls fetch a block already fetched on an earlier turn.
- **Supersedes:** `offload-vs-retrieval-3x.md` for this window. That audit
  priced 441 continuations; this window has 2,220 rounds and 3,517 calls.

## Detail

Input-token equivalents, API weights, `--ctx-offload-min-bytes 2000`:

| | equivalents | note |
|---|---|---|
| saved | 57.1M | proxy `tokens_saved` × 0.10; the transcript model gives 51.7M |
| retrieval | 67.1M | 2,220 hidden rounds, 1.7 h of waiting |
| re-read | 4.7M | 18.7% excess follow-up × 2,915 offloaded file reads |
| net | −14.7M | |

**The re-read jump is sharp at the cut.** File reads just under 2,000 bytes go
back to the same file within two calls 53.0% of the time (n=2,281); just over,
71.6% (n=1,097).

**Retrieval grows with block size.** Calls per offloaded block: 0.21 at
2–4K, 0.70 at 4–8K, 1.18 at 8–16K, 1.84 at 16–64K.

**Most retrieval calls repeat an earlier one.** 4,704 calls for 1,467 distinct
hashes over 2026-09-23..24; 3,149 of them fetch a hash an earlier request had
already fetched. The continuation happens out of the client's sight, so the
content never enters its history: next turn the model sees the digest again
and fetches it again.

**Threshold sweep.** In 2,000-byte steps, net turns positive near 10,000
bytes and peaks at 20,000 (+2.7M API, +2.9M subscription), flat from 18,000
to 22,000. Set live 2026-09-24. Even the best threshold saves little next to
what the repeat-retrieval flaw costs at 2,000. The 16–24K bucket behind the
peak holds 143 blocks, so re-run the sweep before moving it again.

**Zen.** 1,097 of the rounds in 2026-09-23..24 ran on the free Zen route,
where offload saves no money. `--ctx-offload-zen` (default off) now skips it
there.
