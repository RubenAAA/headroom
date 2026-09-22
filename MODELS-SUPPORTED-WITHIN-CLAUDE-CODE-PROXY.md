# Models Supported Within Claude Code via the Proxy

Switch models inside a session with `/model <alias>`. The alias routes through
the proxy; the session, tools, and cache stay intact.

| Type `/model ...` | Real model | Route | Subagent |
|---|---|---|---|
| `claude-codex-5.6-sol` | GPT-5.6 sol | OpenAI | `codex-sol` |
| `claude-codex-5.6-luna` | GPT-5.6 luna | OpenAI | `codex-luna` |
| `claude-codex-5.6-terra` | GPT-5.6 terra | OpenAI | `codex-terra` |
| `claude-codex-6-astra` | GPT-6 astra | OpenAI | `codex-6-astra` |
| `claude-codex-6-sol` | GPT-6 sol | OpenAI | `codex-6-sol` |
| `claude-codex-6-luna` | GPT-6 luna | OpenAI | `codex-6-luna` |
| `claude-grok-4.6-high` | Grok 4.6 | Cursor subscription | `grok-high` |
| `claude-grok-4.6-low` | Grok 4.6 Low | Cursor subscription | `grok-low` |
| `claude-grok-4.6-xhigh` | Grok 4.6 Extra High | Cursor subscription | `grok-xhigh` |
| `claude-muse-spark-1.3` | Muse Spark 1.3 | OpenCode Zen, free tier | `spark`, `spark-explore` |
| `claude-union-alpha` | Union Alpha Free | OpenCode Zen, Anthropic Messages | — |

Notes:

- Codex aliases translate to the matching `gpt-5.6-*` or `gpt-6-*` model on
  the OpenAI route (`contrib/headroom-flags.sh` `--extra-model-route` lines).
- Grok aliases ride the Cursor subscription; there is also a
  `claude-grok-4.6` template alias taking an effort suffix.
- Spark is free with no key spent, but the tier has dynamic unpublished rate
  limits, and Meta may train on prompts/completions — keep sensitive files out
  of spark sessions. `spark-explore` is read-only reconnaissance on the same
  model.
- Union Alpha uses Zen's Anthropic Messages endpoint with the
  `OPENCODE_API_KEY` environment variable. Its upstream model ID is `union-alpha`.
- When delegating from Claude Code, invoke the subagent without the `model`
  parameter. Passing one overrides the pinned model and sends the work back to
  a Claude alias.
- Aliases live in `contrib/headroom-flags.sh` (`--extra-model-route`) and the
  agent definitions in `contrib/claude/agents/` (installed to
  `~/.claude/agents/`). If this table and the flag file disagree, the flag
  file wins.
