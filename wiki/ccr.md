# CCR: Compress-Cache-Retrieve

Headroom's CCR architecture makes compression **reversible**. When content is compressed, the original data is cached. If the LLM needs more data, it can retrieve it instantly.

## The Problem with Traditional Compression

Traditional compression is lossy — if you guess wrong about what's important, data is lost forever. This creates a difficult tradeoff:

- **Aggressive compression**: Risk losing data the LLM needs
- **Conservative compression**: Miss out on token savings

CCR eliminates this tradeoff.

## CCR-Enabled Components

| Component | What it compresses | CCR integration |
|-----------|-------------------|-----------------|
| **SmartCrusher** | JSON arrays (tool outputs) | Stores original array, marker includes hash |
| **ContentRouter** | Code, logs, search results, text | Stores original content by strategy |

## How CCR Works

```
┌─────────────────────────────────────────────────────────────────┐
│  TOOL OUTPUT (1000 items)                                        │
│  └─ SmartCrusher compresses to 20 items                         │
│  └─ Original cached with hash=abc123                            │
│  └─ Retrieval tool injected into context                        │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  LLM PROCESSING                                                  │
│  Option A: LLM solves task with 20 items → Done (90% savings)   │
│  Option B: LLM calls headroom_retrieve(hash=abc123)             │
│            → Response Handler executes retrieval automatically  │
│            → LLM receives full data, responds accurately        │
└─────────────────────────────────────────────────────────────────┘
```

### Phase 1: Compression Store

When SmartCrusher compresses tool output:
1. Original content is stored in an LRU cache
2. A hash key is generated for retrieval
3. A marker is added to the compressed output: `[1000 items compressed to 20. Retrieve more: hash=abc123]`

### Phase 2: Tool Injection

Headroom injects a `headroom_retrieve` tool into the LLM's available tools:

```json
{
  "name": "headroom_retrieve",
  "description": "Retrieve original uncompressed data from Headroom cache",
  "parameters": {
    "hash": "The hash key from the compression marker"
  }
}
```

### Phase 3: Response Handler

When the LLM calls `headroom_retrieve`:
1. Response Handler intercepts the tool call
2. Retrieves data from the local cache (~1ms)
3. Adds the result to the conversation
4. Continues the API call automatically

**The client never sees CCR tool calls** — they're handled transparently.

### Answering Every Retrieval

Roughly one retrieval in five used to go unanswered, and an unanswered
`headroom_retrieve` is worse than no retrieval at all: the client gets a
`tool_use` block for a tool it does not have and rejects the turn. Three holes
are now closed.

**Streaming turns.** Retrieval-answering originally existed only on the
buffered path, so every interactive client — all of which stream — got
`No such tool available: headroom_retrieve`. The SSE rewriter now watches
`content_block_start` for proxy-owned tools, suppresses those blocks, and holds
back `message_delta` and `message_stop` to the end of the stream. If a
retrieval appeared, it rebuilds the message, resolves it through the same
handler the buffered path uses, and synthesizes fresh SSE events for whatever
survives. Blocks already sent live are not re-sent, and `thinking` blocks from
the continuation are dropped — their signatures will not verify on replay. This
covers clients on Anthropic `/v1/messages`, including routed local models;
native OpenAI-shaped streaming clients still fall back to buffered handling.

**Retrieval mixed with a real tool call.** The handler used to give up on the
whole turn when the model asked for a retrieval *and* a genuine tool in one
response. Now the retrieval block is replaced in place with a text block
wrapped in `<retrieved_context>`, and the real tool call is passed through to
the client untouched.

**Transient upstream failures and bad hashes.** The continuation request retries
twice with exponential backoff from 250ms on transport errors, 5xx, and 429;
4xx is not retried. A `headroom_retrieve` call with a malformed or missing hash
now still counts as a retrieval — the lookup misses and the model gets an error
result, instead of the call being handed to the client as an ordinary tool. Any
retrieval that is still unresolved when the rounds run out gets spliced in as a
failure message rather than left dangling, and a turn that claims
`stop_reason: tool_use` with no surviving tool block is downgraded to
`end_turn`.

Outcomes are counted by `proxy_ccr_retrieval_outcomes_total{outcome}`
(`continuation`, `spliced_mixed`, `unresolved`) and
`proxy_ccr_continuation_retries_total`.

### Offload and `--exclude-tools`

`--exclude-tools` no longer gates offload. Exclusion exists to keep the
live-zone compressors — which rewrite content and keep no original — away from
a file the model is about to edit. Offload keeps the original retrievable, so
nothing is destroyed and the argument does not carry over. About 1.8x more
blocks are offload-eligible as a result, and the context tracker's capacity was
raised to match. The stricter verbatim exclusion, for results that break on any
byte change, still applies at every distance. `ctx_offloaded_blocks_by_tool_total{tool}`
breaks offloaded blocks down by source tool.

### Phase 4: Context Tracker

Across multiple turns, the Context Tracker:
1. Remembers what was compressed in earlier turns
2. Analyzes new queries for relevance to compressed content
3. Proactively expands relevant data before the LLM asks

**Example:**
```
Turn 1: User searches for files
        → Tool returns 500 files
        → SmartCrusher compresses to 15, caches original (hash=abc123)
        → LLM sees 15 files, answers question

Turn 5: User asks "What about the auth middleware?"
        → Context Tracker detects "auth" might be in abc123
        → Proactively expands compressed content
        → LLM sees full file list, finds auth_middleware.py
```

## CCR Stores Content Blocks, Not Dropped Messages

Headroom never drops whole messages from conversation history. CCR is purely about compressed **content blocks** — the newest tool outputs, tool results, and user content that the live-zone pipeline compresses. The original block is stored in the cache and is retrievable on demand:

```
┌─────────────────────────────────────────────────────────────────┐
│  LATEST TOOL RESULT (500 files, 12K tokens)                      │
│  └─ ContentRouter / SmartCrusher compresses the block           │
│  └─ Original cached with hash=def456                            │
│  └─ Marker inserted: "500 items compressed, retrieve: def456"   │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  LLM PROCESSING                                                  │
│  Option A: LLM solves task with the compressed block → Done     │
│  Option B: LLM needs the full content                           │
│            → Calls headroom_retrieve(hash=def456)               │
│            → Full original block restored                        │
└─────────────────────────────────────────────────────────────────┘
```

The older conversation turns, system prompt, and tool definitions — the provider cache hot zone — are never mutated, so prompt caching keeps working. Compression happens only on the live zone (the newest content blocks) and is fully reversible via CCR.

**TOIN integration:** When users retrieve compressed content, TOIN learns to treat those patterns as higher value next time, improving future compression decisions across all users.

## Features

| Feature | Description |
|---------|-------------|
| **Automatic Response Handling** | When LLM calls `headroom_retrieve`, the proxy handles it automatically |
| **Multi-Turn Context Tracking** | Tracks compressed content across turns, proactively expands when relevant |
| **Hash-Keyed Retrieval** | `headroom_retrieve(hash)` always returns the full original content |
| **Feedback Learning** | Learns from retrieval patterns to improve future compression |

## Configuration

```bash
# Proxy with CCR enabled (default)
headroom proxy --port 8787

# Tool injection and the compression marker (both on by default)
headroom proxy --ccr-inject-tool --ccr-inject-marker

# Cap how many retrieval rounds one turn may take (default 8)
headroom proxy --ccr-max-retrieval-rounds 8

# Multi-turn context tracking and proactive expansion
headroom proxy --ccr-context-tracking --ccr-proactive-expansion
headroom proxy --ccr-max-proactive-expansions 2
```

## Why This Matters

| Approach | Risk | Savings |
|----------|------|---------|
| No compression | None | 0% |
| Traditional compression | Data loss | 70-90% |
| CCR compression | None (reversible) | 70-90% |

CCR gives you the savings of aggressive compression with zero risk — the LLM can always retrieve the original data if needed.

## Demo

Run the CCR demonstration to see it in action:

```bash
python examples/ccr_demo.py
```

Output:
```
1. COMPRESSION STORE
   Original: 100 items (7,059 chars)
   Compressed: 8 items (633 chars)
   Reduction: 91.0%

3. RESPONSE HANDLER
   Detected CCR tool call: True
   Retrieved 100 items automatically

4. CONTEXT TRACKER
   Turn 5: User asks "show authentication middleware"
   Tracker found 1 relevant context
   → relevance=0.73
   Proactively expanded: 100 items
```

## Architecture

For implementation details, see [ARCHITECTURE.md](ARCHITECTURE.md#ccr-compress-cache-retrieve).
