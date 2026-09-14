# Learning: output bucket composition bounds the verbosity lever

- **Source:** `docs/notes/savings-ideas-1.md` (two large transcripts)
- **Claim:** what the client sends — thinking 31–35%, tool_result 28–30%, Bash
  input 22%, assistant text <5%, user text 3–7%. Per assistant turn ~430
  thinking / 200 tool_use / 100 text tokens; 86% of output lands on tool_use
  turns. Any text-only lever caps near $5/day of the $83 output bucket.


## Detail

*moved from `docs/notes/savings-ideas-1.md`*

Composition of what the client sends, from two large Claude Code transcripts
(bytes): thinking 31-35%, tool_result 28-30%, Bash `tool_use` input 22%,
assistant text 4.6-4.8%, user text 3-7%.
