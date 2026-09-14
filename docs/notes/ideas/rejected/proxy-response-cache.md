# Rejected: proxy response cache

- **Status:** rejected 2026-09-03 (measured)
- **Source:** `docs/notes/savings-ideas-1.md` (ruled out)
- **Summary:** only non-streaming bodies qualify and every Claude Code turn
  streams — zero `semantic_cache_hit` in 541k lines. 13 identical bodies in
  four days: $0.03.


## Detail

*moved from `docs/notes/savings-ideas-1.md`*

- **Proxy response cache** (`--cache true`): only non-streaming bodies qualify
  (`proxy.rs:3266-3284`) and every Claude Code turn streams. Zero
  `semantic_cache_hit` events in 541k lines. Identical bodies (same
  conversation, message count, forwarded bytes, model, within 10 minutes): 13
  in four days, $0.03.
