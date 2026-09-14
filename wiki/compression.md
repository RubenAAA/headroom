# Compression

!!! note "Live implementation: Rust"
    The production proxy is the Rust binary (`crates/headroom-proxy`, launched with `cclaude`). Python paths on this page now live in the read-only `upstream-python/` mirror — re-resolve any `headroom/*.py` cite there. Behavior described here still holds; only the implementation moved.

Compression shrinks the wasteful parts of LLM traffic — huge repetitive
tool outputs — deterministically (statistical analysis and rules, no LLM
calls), inside the proxy, before the request goes upstream. It touches
only the newest content blocks, never the cached prefix, and every lossy
step is reversible via CCR.

This page is the concept and the what-runs-where map. The stage-by-stage
pipeline order lives in [ARCHITECTURE.md](ARCHITECTURE.md); per-transform
knobs live in the [Transform Reference](transforms.md).

## What compresses

The live-zone dispatcher (`crates/headroom-core/src/transforms/live_zone.rs`)
detects each eligible block's content type and routes it to one compressor:

| Content | Compressor | Module |
|---------|-----------|--------|
| JSON arrays | SmartCrusher | `crates/headroom-core/src/transforms/smart_crusher/` |
| Build / test / log output | LogCompressor | `crates/headroom-core/src/transforms/log_compressor.rs` |
| Grep / ripgrep output | SearchCompressor | `crates/headroom-core/src/transforms/search_compressor.rs` |
| Unified diffs | DiffCompressor | `crates/headroom-core/src/transforms/diff_compressor.rs` |
| Source code | CodeCompressor | `crates/headroom-core/src/transforms/code_compressor.rs` |
| Plain prose | Kompress (ML, opt-in) | `crates/headroom-core/src/transforms/kompress.rs` |
| HTML | No-op (extraction, not compression) | — |

Two gates keep the dispatcher honest: blocks under the ~512-byte
per-type floor are skipped without even spinning up a compressor, and a
tokenizer check rejects any output that is not strictly smaller in tokens
(`compressed.tokens >= original.tokens` falls back to the original bytes).

### Detection

Detection is regex-native by default
(`crates/headroom-core/src/transforms/content_detector.rs`, dispatched via
`crates/headroom-core/src/transforms/content_router.rs`): JSON shape, code
syntax, `file:line:` patterns, timestamps and log levels, diff headers.
Magika (`crates/headroom-core/src/transforms/magika_detector.rs`, behind
the `ml` feature) acts as an ML Tier 1 classifier whose labels map onto
the same `ContentType` enum; failures fall through to the regex tiers
rather than silently degrading to plain text.

### What is preserved

Every compressor preserves structure and drops bulk:

| Content Type | What's Preserved | What's Compressed |
|--------------|------------------|-------------------|
| **JSON** | Keys, brackets, booleans, nulls, short values, UUIDs | Long string values, whitespace, redundant array items |
| **Code** | Imports, function signatures, class definitions, types | Function bodies, comments |
| **Logs** | Timestamps, log levels, error messages | Repeated patterns, verbose details |
| **Text** | High-entropy tokens (IDs, hashes) | Low-information content |

Safety invariants (shared with all transforms): human content is never
removed, tool call/result pairing is never broken, parse failures pass
content through untouched, and error items are always kept. See
[Transform Reference](transforms.md#safety-guarantees).

## Where in the pipeline it runs

Compression is one stage of the request path described in
[ARCHITECTURE.md](ARCHITECTURE.md). Its neighbors matter:

- **Sidecar short-circuit comes first**
  (`crates/headroom-proxy/src/sidecar.rs`). Claude Code's spinner-text
  sidecar resends the conversation to render a status line; the proxy
  answers it with a tail-only request to a small model before any
  pipeline state runs, so it neither bills a full prefix read nor
  poisons the replay store.
- **CTX offload runs before live-zone compression**
  (`crates/headroom-proxy/src/compression/ctx_offload.rs`). Oversized
  `tool_result` blocks are replaced with `<<ctx:…>>` digests and the
  originals persist into the CCR store. The live zone recognizes those
  digests and skips them — re-compressing a digest would buy almost
  nothing and bury the true bytes under a second marker.
- **Live-zone compression rewrites only the newest blocks**
  (`crates/headroom-proxy/src/compression/live_zone_anthropic.rs`,
  `live_zone_openai.rs`, `live_zone_responses.rs` calling into
  `crates/headroom-core/src/transforms/live_zone.rs`). Everything outside
  the rewritten ranges is spliced through byte-identical, so the cached
  prefix the provider already holds stays stable.
- **Prefix replay runs after**
  (`crates/headroom-proxy/src/cache_stabilization/prefix_replay.rs`).
  The previously *forwarded* (compressed) prefix is replayed byte-for-byte
  when the turn append-only-extends the last one; only the new delta goes
  out as fresh compressor output. Deterministic output matters here:
  identical content must compress to identical bytes turn after turn, or
  the prefix oscillates and the provider cache cascades.

!!! warning "Reversibility is a requirement, not a feature"
    Each rewritten block stores its original under a hash and appends a
    `<<ccr:HASH>>` marker the model can redeem with `headroom_retrieve`.
    See [CCR](ccr.md) for the operator-facing details.

## Kompress (opt-in ML prose compression)

SmartCrusher/Log/Search/Diff/Code are deterministic and always on.
Kompress is the ML exception: a ModernBERT token-salience model (~261 MB
ONNX) that keeps only the words the model scores as salient. Because of
its size it is opt-in at startup (`--enable-kompress`), loads cache-only
(never downloads on the request path), and warms off-path via
`warm_live_zone_compressors` — until warming completes, plain-text blocks
pass through. When disabled or uncached, prose simply skips compression;
nothing else changes.

## Observing savings

- **`GET /stats`** (`crates/headroom-proxy/src/handlers/stats.rs`) is the
  operator view: cost totals, the durable `persistent_savings` ledger
  (backed by `crates/headroom-core/src/savings_ledger.rs` alongside
  `crates/headroom-core/src/cost_tracker.rs`), savings/wire verdicts,
  per-tool inventory, proxy overhead, and recent requests. History lives
  at `/stats-history`; `headroom savings` renders the same ledger from
  disk without a running proxy.
- **`turn_cost_ledger`** (`crates/headroom-proxy/src/cache_stabilization/usage_observer.rs`)
  is the per-turn structured-log ledger joining billed usage across
  retrieval and continuation rounds, so one turn's true cost is visible
  in one place.
- **`GET /cache-health`** is the cache watchdog snapshot (hit rates,
  re-cache counts, upstream health) — the proof that compression is not
  busting the provider prefix cache it exists to protect.

> **Sourced numbers live elsewhere:** illustrative token counts are
> deliberately not repeated here. For measured compression figures see
> [Benchmarks](benchmarks.md); for the metrics surface see
> [Metrics](metrics.md).

## See Also

- [Transform Reference](transforms.md) — per-transform behavior and configuration
- [ARCHITECTURE.md](ARCHITECTURE.md) — the full request pipeline
- [CCR](ccr.md) — reversible compression and retrieval
- [Text Compression](text-compression.md) — opt-in utilities for search/logs
