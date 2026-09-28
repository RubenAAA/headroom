# Idea: sparse line numbers + repeated-overhead strip (§5)

- **Status:** rejected 2026-09-28 — about 1.3% of cache writes, and the
  cost lands on `file:line` citations.
- **Source:** `LOOK_AT_THIS_WHEN_YOU_HAVE_TIME.md` §5 (every-10th-line numbering cut cache-read tokens 1.6%; also paths, JSON keys, ANSI, progress bars, headers). Proxy: `content_detector.rs:236-251` strips numbers for detection, `cross_turn_dedup.rs:180-253` keys on content and folds renumbered re-reads — no emitter produces sparse numbers.
- **Honest scope:** subscription reads are free (`measurement.md:6-8`), so a pure read-discount win is ~$0 there; `cost-saves-measured.md` caps all compression near 1.5% of bill on a 91%-cached workload. Price in both weights (creation + PAYG reads), never raw tokens. This shrinks first writes; dedup folds re-reads — complementary, not duplicate. `traffic-beats-theory-twice.md` puts the addressable bytes at the tool boundary, which is where this fires. `output-bucket-composition.md` bounds it: tool_result 28–30%, Bash input 22% — numbers live inside those legs.
- **Hard constraint from a rejection:** `rejected/partial-prefix-replay.md` (204,768 tokens on first firing: a declined replay is not a bust; the seam matches neither turn). So this NEVER rewrites mid-prefix: greenfield sessions/rebuild boundaries only (same gate as thinking-drop/offload), deterministic same-content → same-bytes so replay stays intact.
- **Lossy/lossless placement:** `lossless-only-mode-ab.md` (open) argues lossless is stabler over time. Default to the lossless subset (number thinning, ANSI/strip, path-shorten without meaning change); any lossy step needs its own A/B.
- **Next:** audit one week of Read/Grep/Search output for number/path/ANSI/header share in both weights. One deterministic pass behind a flag. Validate citation accuracy, turns/task flat, depth-binned creation delta per `recache-counting-rules.md`.
- **Exit:** ship on creation-cost drop with no quality signal; reject with the number if neutral.
- **Tool:** `crates/headroom-proxy/src/bin/sparse_overhead_audit.rs` (offline; measures number-prefix, path-span, and ANSI token shares inside `tool_result` blocks per category, plus a simulated every-10th-line saving. Written 2026-09-24, unverified — tree was mid-refactor; run on netvalue/blindguard once green).

## Findings 2026-09-28 — too small for what it breaks

`sparse_overhead_audit`, share of `tool_result` tokens that numbering every
10th line would save:

| capture | category | tokens | line-number share | sparse saving |
|---|---|---|---|---|
| netvalue (4,528 turns) | file | 30.7M | 6.5% | 13.8% |
| netvalue | command | 124.6M | 0.9% | 0.7% |
| baseline-202609 (483 turns) | file | 0.23M | 7.4% | 19.2% |
| baseline-202609 | command | 13.6M | 0.9% | 1.0% |

`result:file` is 9.4% of netvalue's write tokens (`section_cost_baseline`),
so the saving is about 1.3% of writes, plus about 0.2% from commands. ANSI
codes are negligible (68KB in 124M tokens). The saving comes almost wholly
from Read output, and Read's line numbers are what the model cites as
`file:line` and uses to set `offset`. Dropping nine in ten turns those into
guesses. Not worth an A/B for 1.5%.
