# Learning: offload-gap loose ends (ruled out / unexplained)

- **Source:** `docs/notes/proxy-experiments-2026-08.md` (offload-gap round)
- **Claim:** J4 gate withholds nothing; thinking-strip unshippable (guard restores); images price by dimensions; jemalloc lost on real proxy (RSS is live data); tokenize/fsync don't dominate; replay-boundary 3.1%-vs-0.16% is a denominator mismatch; near-tail window fires 1/4.8 turns.


## Detail

*moved from `docs/notes/proxy-experiments-2026-08.md`*

Ruled out by measurement, so nobody re-checks:

- **The PR-J4 boundary gate withholds nothing.** With the boundary requirement
  and without it, −6.6% either way, 0.01% apart.
- **Stripping old thinking blocks** scores −3.2% and is unshippable.
  `restore_client_reasoning_blocks` compares outbound signed `thinking` and
  `redacted_thinking` blocks against the client's and restores the whole message
  array on any mismatch, and a deletion is a mismatch, so the arm would be
  reverted every turn. The guard exists because Anthropic rejects the turn
  outright.
- **Byte-based prompt composition.** Images bill by dimensions, not base64
  length — 9.3 to 11.1 bytes per token against 3.1 for text — so the 2 MB image
  bodies are the cheap ones. Price tokens before claiming what dominates a
  prompt.
- **Swapping the allocator.** A synthetic benchmark parsing a real 908 KB body
  on 20 threads made jemalloc look like a 3.5x memory win that was also faster
  (0.57s and +65 MB retained, against glibc's 0.90s and +230 MB). Measured on the
  real proxy — 60 captured bodies through a dummy upstream with offload, inject,
  memory, compression and prefix-replay all on — jemalloc peaked at 325 MB
  against glibc's 290 MB, and `background_thread:true,dirty_decay_ms:2000` only
  brought it to 312 MB. The change was reverted. The flat settle curve from 10s
  to 60s is the real finding: nothing decays because nothing is waiting to be
  freed, and the proxy's RSS is live data held on purpose by the replay store,
  the offload store, the CCR tracker and the semantic cache. A microbenchmark
  that models the wrong allocation lifetime will confidently recommend the wrong
  fix. The levers that would work all trade cache coverage for bytes, which was
  out of scope.
- **Tokenization, fsync and quadratic scaling in message count** are not where
  the proxy's own time goes: 0.9 ms to tokenize 710 KB, since Claude models
  resolve to the estimator rather than BPE; 2.3 ms for fsync on this filesystem;
  and cost per message *falls* from 4,168 us at 20 messages to 361 us at 586.


## Unexplained

*moved from `docs/notes/proxy-experiments-2026-08.md`*

Left unexplained, and worth knowing before quoting a replay counter:

- **The replay sees a rebuild boundary on 3.1% of turns where production reported
  0.16%.** 245 of 7,839 on blindguard, reproduced twice. It does not affect any
  before/after comparison that holds the detector fixed across both runs, but do
  not read the absolute deferral counts as production truth until it is chased
  down. For scale, replaying blindguard across `4f223054` and its parent moved
  `blocks_offloaded` from 72,349 to 73,181, `blocks_deferred` from 18,453 to
  18,441, `window_offloads` from 1,681 to 1,673 and `tokens_saved` from
  95,335,968 to 97,392,897 — about +1.1% blocks and +2.2% tokens saved, real but
  not the step change that had been predicted. Deferrals do not move.
- **The near-tail window is not inert**, which an earlier note had claimed at 1
  window offload per 11 turns. Over the 08-17 to 08-19 log span, 8,287 turns
  carrying a qualifying block, production fires 1,742 window offloads, 1 per 4.8
  turns. The replay agrees independently at 1 per 4.7 on blindguard and 1 per 5.4
  on windowgap. The old figure divided a count from one span by a turn count from
  a wider one.


## Store paths

*moved from `docs/notes/proxy-experiments-2026-08.md`*

Where the memory store actually lives, since two paths and a decoy made this
expensive to establish:

- `~/.claude-personal/context-mode` is a symlink to `~/.claude-work/context-mode`
  and the proxy runs with `--ctx-store-dir` pointed at the former, so both paths
  are one physical store at
  `~/.claude-work/context-mode/memory/memories.db`. `~/.headroom/memories` is
  `default_native_memory_dir()`, used only when `use_native_tool` is on, and it
  is empty.
- `user_id` is carried twice, in the column and inside the record JSON. Update
  both or reads go inconsistent.
- The `workspace` partition is empty as of 2026-08-19. `default` is the right
  home for cross-repo reference, because `router::shared_partition` strips the
  `::project` suffix, so a record stored under plain `default` is visible from
  every project partition (`ctx_backend.rs` line 140).
