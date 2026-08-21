# Transform Reference

Headroom provides several transforms that work together to optimize LLM context.

## SmartCrusher

Statistical compression for JSON tool outputs.

### How It Works

SmartCrusher analyzes JSON arrays and selectively keeps important items:

1. **First/Last items** - Context for pagination and recency
2. **Error items** - 100% preservation of error states
3. **Anomalies** - Statistical outliers (> 2 std dev from mean)
4. **Relevant items** - Matches to user's query via BM25/embeddings
5. **Change points** - Significant transitions in data

### Adaptive Sizing

How many items to keep is not a fixed number. A ten-item array and a
ten-thousand-item array need different budgets, and so do a repetitive log dump
and a list where every row says something new.

SmartCrusher builds a cumulative unique-bigram coverage curve over the items in
importance order — each item contributes its word-level bigrams, and the curve
records how much new vocabulary has appeared by item *i*. That curve rises fast
at first, then flattens once the array starts repeating itself. Kneedle finds
where it bends: normalize both axes to `[0,1]`, then take the point of greatest
deviation from the diagonal. That knee is where more items stop buying new
information, and it becomes the item budget.

Three tiers guard it:

- **8 items or fewer**, or 3 or fewer distinct items by SimHash, pass through
  without analysis.
- **A knee under 0.05 deviation doesn't count** — that curve is too straight to
  have a bend. The budget falls back to `n * (0.3 + 0.7 * diversity)`, where
  diversity is the fraction of items that are genuinely distinct. The same
  expression acts as a floor whenever diversity runs above 0.7, so a list where
  every row differs is not cut to the knee.
- **A zlib check** then raises the budget by about 20% if the kept items
  compress much better than the full array, which means the sample was more
  redundant than the whole.

A caller-supplied `bias` multiplier scales the knee — above 1 keeps more, below
1 compresses harder. The result is clamped to a floor of 3 and to
`max_items_after_crush`, then split: `first_fraction` to the head of the array,
`last_fraction` to the tail, and the rest to whatever scored as important.

### Configuration

```python
from headroom import SmartCrusherConfig

config = SmartCrusherConfig(
    min_items_to_analyze=5,       # Don't analyze arrays smaller than this
    min_tokens_to_crush=200,      # Only compress if > 200 tokens
    max_items_after_crush=15,     # Cap on the adaptive item budget
    first_fraction=0.3,           # Share of the budget for the head
    last_fraction=0.15,           # Share of the budget for the tail
    variance_threshold=2.0,       # Std devs from mean to count as a change point
    uniqueness_threshold=0.1,     # Below this unique-ratio a field is near-constant
    similarity_threshold=0.8,     # Above this, strings cluster together
    relevance_threshold=0.3,      # Pin items scoring above this against the query
    preserve_change_points=True,  # Keep detected transitions
    dedup_identical_items=True,   # Drop identical items before sampling
    use_feedback_hints=True,      # Let retrieval feedback bias the budget
    toin_confidence_threshold=0.5,# Min confidence to apply a TOIN recommendation
    lossless_min_savings_ratio=0.15,  # Below this, fall through to the lossy path
)
```

`max_items_after_crush` is a ceiling, not a target — the adaptive sizer usually
lands below it. Set it to `0` to remove the cap entirely and let the knee decide.

### Example

```python
from headroom import SmartCrusher

crusher = SmartCrusher(config)

# Before: 1000 search results (45,000 tokens)
tool_output = {"results": [...1000 items...]}

# After: ~15 important items (1,400 tokens) - 97% reduction
compressed = crusher.crush(tool_output, query="user's question")
```

### What Gets Preserved

| Category | Preserved | Why |
|----------|-----------|-----|
| Errors | 100% | Critical for debugging |
| First N | 100% | Context/pagination |
| Last N | 100% | Recency |
| Anomalies | All | Unusual values matter |
| Relevant | Top K | Match user's query |
| Others | Sampled | Statistical representation |

---

## CacheAligner

Prefix stabilization for improved cache hit rates.

### The Problem

LLM providers cache request prefixes. But dynamic content breaks caching:

```
"You are helpful. Today is January 7, 2025."  # Changes daily = no cache
```

### The Solution

CacheAligner extracts dynamic content to stabilize the prefix:

```python
from headroom import CacheAligner

aligner = CacheAligner()
result = aligner.align(messages)

# Static prefix (cacheable):
# "You are helpful."

# Dynamic content moved to end:
# [Current date context]
```

### Configuration

```python
from headroom import CacheAlignerConfig

config = CacheAlignerConfig(
    extract_dates=True,           # Move dates to dynamic section
    normalize_whitespace=True,    # Consistent spacing
    stable_prefix_min_tokens=100, # Min prefix size for alignment
)
```

### Cache Hit Improvement

| Scenario | Before | After |
|----------|--------|-------|
| Daily date in prompt | 0% hits | ~95% hits |
| Dynamic user context | ~10% hits | ~80% hits |
| Consistent prompts | ~90% hits | ~95% hits |

---

## Context management

Context management is handled automatically inside the pipeline
(live-zone-only compression). Headroom **never** drops messages from the
conversation history and does not do position-based or score-based context
management. It compresses only the newest content blocks (the latest user
message and the latest tool result / tool output), type-aware and reversible
via CCR. The cache hot zone — system prompt, tools, and older turns — is never
mutated, which preserves provider prompt caching.

> The earlier position-based `RollingWindow` and score-based
> `IntelligentContextManager` transforms have been removed and are no longer
> part of Headroom.

---

## LLMLinguaCompressor — RETIRED

The earlier LLMLingua-2 integration (`LLMLinguaCompressor`,
`LLMLinguaConfig`, `is_llmlingua_model_loaded`, `unload_llmlingua_model`,
the `headroom-ai[llmlingua]` extra, and the `--llmlingua` proxy flag)
was retired in 0.9.x and replaced by **Kompress** (ModernBERT).
`pip install 'headroom-ai[llmlingua]'` no longer resolves; use the
`[ml]` extra instead. The Kompress transform shipped with the proxy
runs as Transform 4 in the live-zone pipeline (see
[ARCHITECTURE.md](ARCHITECTURE.md)).

---

## CodeAwareCompressor (Optional)

AST-based compression for source code using tree-sitter.

### When to Use

| Transform | Best For | Speed | Compression |
|-----------|----------|-------|-------------|
| SmartCrusher | JSON arrays | ~1ms | 70-90% |
| **CodeAwareCompressor** | Source code | ~10-50ms | 40-70% |
| Kompress (ML) | Any text | 50-200ms | 80-95% |

### Key Benefits

- **Syntax validity guaranteed** — Output always parses correctly
- **Preserves critical structure** — Imports, signatures, types, error handlers
- **Multi-language support** — Python, JavaScript, TypeScript, Go, Rust, Java, C, C++
- **Lightweight** — ~50MB vs ~1GB for the ML compressor

### Installation

```bash
pip install "headroom-ai[code]"  # Adds tree-sitter-language-pack
```

### Configuration

```python
from headroom.transforms import CodeAwareCompressor, CodeCompressorConfig, DocstringMode

config = CodeCompressorConfig(
    preserve_imports=True,              # Always keep imports
    preserve_signatures=True,           # Always keep function signatures
    preserve_type_annotations=True,     # Keep type hints
    preserve_error_handlers=True,       # Keep try/except blocks
    preserve_decorators=True,           # Keep decorators
    docstring_mode=DocstringMode.FIRST_LINE,  # FULL, FIRST_LINE, REMOVE
    target_compression_rate=0.2,        # Keep 20% of tokens
    max_body_lines=5,                   # Lines to keep per function body
    min_tokens_for_compression=100,     # Skip small content
    language_hint=None,                 # Auto-detect if None
)

compressor = CodeAwareCompressor(config)
```

### Example

```python
from headroom.transforms import CodeAwareCompressor

compressor = CodeAwareCompressor()

code = '''
import os
from typing import List

def process_items(items: List[str]) -> List[str]:
    """Process a list of items."""
    results = []
    for item in items:
        if not item:
            continue
        processed = item.strip().lower()
        results.append(processed)
    return results
'''

result = compressor.compress(code, language="python")
print(result.compressed)
# import os
# from typing import List
#
# def process_items(items: List[str]) -> List[str]:
#     """Process a list of items."""
#     results = []
#     for item in items:
#     # ... (5 lines compressed)
#     pass

print(f"Compression: {result.compression_ratio:.0%}")  # ~55%
print(f"Syntax valid: {result.syntax_valid}")  # True
```

### Supported Languages

| Tier | Languages | Support Level |
|------|-----------|---------------|
| 1 | Python, JavaScript, TypeScript | Full AST analysis |
| 2 | Go, Rust, Java, C, C++ | Function body compression |

### Memory Management

```python
from headroom.transforms import is_tree_sitter_available, unload_tree_sitter

# Check if tree-sitter is installed
print(is_tree_sitter_available())  # True/False

# Free memory when done (parsers are lazy-loaded)
unload_tree_sitter()
```

---

## ContentRouter

Intelligent compression orchestrator that routes content to the optimal compressor.

### How It Works

ContentRouter analyzes content and selects the best compression strategy:

1. **Detect content type** — JSON, code, logs, search results, plain text
2. **Consider source hints** — File paths, tool names for high-confidence routing
3. **Route to compressor** — SmartCrusher, CodeAwareCompressor, SearchCompressor, etc.
4. **Log decisions** — Transparent routing for debugging

### Configuration

```python
from headroom.transforms import ContentRouter, ContentRouterConfig, CompressionStrategy

config = ContentRouterConfig(
    min_section_tokens=100,             # Minimum tokens to compress
    enable_code_aware=True,             # Use CodeAwareCompressor for code
    enable_search_compression=True,     # Use SearchCompressor for grep output
    enable_log_compression=True,        # Use LogCompressor for logs
    default_strategy=CompressionStrategy.TEXT,  # Fallback strategy
)

router = ContentRouter(config)
```

### Example

```python
from headroom.transforms import ContentRouter

router = ContentRouter()

# Router auto-detects content type and routes to optimal compressor
result = router.compress(content)

print(result.strategy_used)  # CompressionStrategy.CODE_AWARE, SMART_CRUSHER, etc.
print(result.routing_log)  # List of routing decisions
```

### Compression Strategies

| Strategy | Used For | Compressor |
|----------|----------|------------|
| CODE_AWARE | Source code | CodeAwareCompressor |
| SMART_CRUSHER | JSON arrays | SmartCrusher |
| SEARCH | Grep/find output | SearchCompressor |
| LOG | Log files | LogCompressor |
| TEXT | Plain text | TextCompressor |
| PASSTHROUGH | Small content | None |

(The earlier `LLMLINGUA` strategy was retired with the LLMLingua integration; ML compression is now provided by Kompress.)

### Content Detection

The router automatically detects content types by analyzing the content itself:

- **Source code**: Detected by syntax patterns, indentation, keywords
- **JSON arrays**: Detected by JSON structure with array elements
- **Search results**: Detected by `file:line:` patterns
- **Log output**: Detected by timestamp and log level patterns
- **Plain text**: Fallback for prose content

No manual hints required - the router inspects content directly.

### TOIN Integration

ContentRouter records all compressions to TOIN (Tool Output Intelligence Network) for cross-user learning:

- **All strategies tracked**: Code, search, logs, text, and ML compressions are recorded
- **Retrieval feedback**: When users retrieve original content via CCR, TOIN learns which compressions need expansion
- **Pattern learning**: TOIN builds signatures for each content type to improve future compressions

This enables the feedback loop where compression decisions improve based on actual user behavior across all content types, not just JSON arrays.

---

## TransformPipeline

Combine transforms for optimal results.

```python
from headroom import TransformPipeline, SmartCrusher, CacheAligner

pipeline = TransformPipeline([
    SmartCrusher(),      # First: compress tool outputs
    CacheAligner(),      # Then: stabilize prefix
])

result = pipeline.transform(messages)
print(f"Saved {result.tokens_saved} tokens")
```

### With ML compression (Optional, Kompress)

The earlier hand-assembled `TransformPipeline([..., LLMLinguaCompressor(), ...])` recipe is no longer supported. ML compression now ships as part of the live-zone pipeline when the `[ml]` extra is installed; see [ARCHITECTURE.md](ARCHITECTURE.md) for the current placement.

### Recommended Order

| Order | Transform | Purpose |
|-------|-----------|---------|
| 1 | CacheAligner | Stabilize prefix for caching |
| 2 | SmartCrusher | Compress JSON tool outputs |
| 3 | Kompress (ML) | ML compression on remaining text (optional, `[ml]` extra) |

**Why this order?**
- CacheAligner first to maximize prefix stability
- SmartCrusher handles JSON arrays efficiently
- Kompress compresses remaining long text

---

## Safety Guarantees

All transforms follow strict safety rules:

1. **Never remove human content** - User/assistant text is sacred
2. **Never break tool ordering** - Calls and results stay paired
3. **Parse failures are no-ops** - Malformed content passes through
4. **Preserves recency** - Last N turns always kept
5. **100% error preservation** - Error items never dropped
