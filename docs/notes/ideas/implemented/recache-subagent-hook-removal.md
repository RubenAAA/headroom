# Implemented: SubagentStart hook reminder unwired

- **Status:** done (config edit, not code)
- **Source:** `docs/notes/recache-classification.md` ("two causes found")
- **Summary:** all three deep divergences were one `role:"system"` message: the
  hook appended its reminder at the tail on every wake and Claude Code deleted
  the older copy, shifting all later messages (one cost 132,539 tokens). The
  text was identical every firing — removed `cbm-subagent-reminder` from
  settings, moved the sentences to the project's `CLAUDE.md` (arrives as
  `<system-reminder>` at the most stable position, verified in captures).


## Detail

*moved from `docs/notes/recache-classification.md`*

## A `SubagentStart` hook deleted a message 48 positions deep

All three deep divergences are the same `role: "system"` message:

```
prior[151] user      <teammate-message teammate_id="team-lead" …>
prior[152] system    SubagentStart hook additional context: Code discovery…
…
cur[199]   user      <teammate-message teammate_id="team-lead" …>
cur[200]   system    SubagentStart hook additional context: Code discovery…
```

A long-lived teammate agent, woken again by `SendMessage`. The hook fires on
every wake and appends its reminder at the tail; Claude Code deletes the older
copy rather than duplicate it. That deletion shifts every message after it, and
one of the three cost 132,539 tokens.

The text was identical on every firing — no per-invocation content at all — so
a hook bought nothing over static context. Removed the `cbm-subagent-reminder`
entry from `acme-api/.claude/settings.local.json` and moved the two sentences to
that project's `CLAUDE.md`.

`CLAUDE.md` does not reach the `system` block, as it happens — it arrives as a
`<system-reminder>` inside `messages[0]`, the front of the array, which is the
most stable position there is. Verified in the capture: all three files
(`~/.claude`, `~/workspace`, `acme-api`) appear there, in 961 of 961 teammate
conversations and 184 of 184 main ones under acme-api. So subagents do get it,
once, at 0.1x forever.

Scoping: `codebase-memory-mcp` runs only in acme-api, so the guidance stays in
that project's `CLAUDE.md` and not in the user-level one. One wiring existed
(the project's `settings.local.json`), which every config dir reads, so
`.claude`, `.claude-personal` and `.claude-work` are all covered by the single
edit. The script itself survives, unwired, at `hooks/cbm-subagent-reminder`
under `.claude-personal` and `.claude-work`; its header says
"Installed by codebase-memory-mcp", so check the wiring again after that
server next installs.
