# Learning: price the 3.9 ms, not the 54

- **Source:** `docs/notes/proxy-followups.md` §5
- **Claim:** 54 ms attributed to tool-stage parses was the savings tracker
  (since fixed); the stages cost 3.9 ms between them, outcome-context 7.5 ms
  on 743 KB. Latency attribution from code reading + size slopes needs a
  measurement gate before it becomes a plan (see also speed plan 2's gate).
