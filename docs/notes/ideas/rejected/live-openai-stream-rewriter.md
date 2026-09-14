# Rejected: live OpenAI-vocabulary stream rewriter

- **Status:** rejected (buffered path chosen instead; Python deferred it too as #1877 B/C)
- **Source:** `docs/notes/rust-parity-gaps.md` §8; `openai_buffered_ccr.rs` docs
- **Summary:** full event-level splicing for chat/Responses SSE was the
  expensive option; forcing `stream:false` upstream + resynthesizing closes the
  same leak with a fraction of the machinery. Revisit only if buffering proves
  inadequate (e.g. TTFT-sensitive clients on retrieve-heavy turns).
