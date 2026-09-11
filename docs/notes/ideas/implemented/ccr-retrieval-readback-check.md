# Idea: verify CCR retrieval through the real read path

- **Status:** answered 2026-09-11 (waste, not breakage — retention cut is a policy call)
- **Source:** `docs/notes/proxy-experiments-2026-08.md` §13
- **Answer:** read path updates `last_accessed` on every hit (`sqlite.rs`
  `get_at`); item 12.1 closed (`observability/ccr_retrieval.rs` live). Live
  store: 12,496 rows, 0.76% ever read, 0 expired. Demand vs fate (today's
  log): 143 `ccr_retrieval_call` → 87 clean successes + 104 continuations
  resolving, ~4 truly unresolved (~3%). Integrity probe: 10/10 exact byte
  match via `GET /ctx/get/:hash`, `last_accessed` advances 5/5. Each outcome
  maps to one action: unresolved≈0 + probes match → shorten TTL / offload
  less; that retention decision is still open.
- **Summary:** 10/10 markers present, plausible lengths, zero expired — but presence isn't integrity, and 532/538 rows were never read back (rarely-needed vs quietly-failing, indistinguishable from outside; cf. item 12.1).
- **Next:** retrieve through the real read path and compare against originals.


## Item 13

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 13. CCR store integrity — checked, no fault found

A correctness risk with no metric, so tested directly. The proxy offloaded
this session's own tool output several times, leaving `<<ccr:HASH>>` markers.
Read-only query against `~/.claude-work/context-mode/ccr.db`:

- 538 rows in `ccr_entries`; **10 of 10 markers observed in live output are
  present**.
- `original` is never null or empty; lengths run 521 B to 133 KB, mean 8.9 KB.
  Nothing suspiciously small.
- Uniform TTL of 604,800 s (7 days); **zero rows expired**. Entries span
  2026-08-02 to now.

Limit of this test: a present row with plausible length proves the entry
exists, not that its content is intact or correctly decompressible. A fuller
check retrieves through the real read path and compares against the original.
Given item 12.1 — the retrieval code has no instrumentation at all — that
fuller check is worth doing before trusting this result.

One number here is worth a second look, though it is not a fault: **532 of
538 rows have `last_accessed <= created_at`**, meaning only 6 entries have
ever been read back. Either offloaded content is rarely needed, in which case
the retrieval machinery is costing more than it returns, or retrieval is
failing quietly and nobody would know, which is exactly item 12.1. The two
explanations are indistinguishable from the outside — another argument for
instrumenting the read path.
