# Model bench

Runs real Claude Code sessions against the routed models, one tmux window
each, through a fixed task suite — and says whether a bad turn was the
model's fault or the proxy's.

```bash
scripts/bench/run-bench.sh --models sonnet,spark,codex-sol,grok-high --tasks T0,T1,T2
tmux attach -t hrbench-<run>
scripts/bench/bench-report.sh <run>
tmux kill-session -t hrbench-<run>
```

`sonnet` is the control. If a task fails there too, the task is wrong, not
the model.

## Why real sessions

`claude -p` would be easier to drive and would miss things that matter. It
skips the spinner sidecar, the statusline and the hooks. The sidecar has
already caused one production stall — see
`docs/notes/learnings/concurrency-stall-is-sidecar-404.md` — so a bench
without it would pass while that class of bug shipped.

## Three instruments

| Source | Answers |
|---|---|
| Artifacts in the scratch dir | Did it do the job. Binary, no judgement. |
| Session transcript JSONL | Tool names **with arguments**, turn timing, blank turns. |
| Proxy log | Continuation failures, 403s, dropped tool calls. |

The transcript is the one that matters most. The proxy log carries tool
names but not their inputs, so from the log alone a model re-reading one
file looks the same as a model exploring a codebase. The transcript has the
arguments, which is what makes real circling detectable.

Transcripts live at `~/.claude-personal/projects/<cwd-with-slashes-as-dashes>/<session-id>.jsonl`.
Each pane is launched with `--session-id`, so its path is known before it
writes anything.

## Tasks

| | Task | Tests |
|---|---|---|
| T0 | reply `READY`, no tools | a turn that produces visible text at all |
| T1 | write a file with exact contents | one tool round trip |
| T2 | count lines, write the number | Read + Bash + arithmetic |
| T3 | fix a failing test without editing it | multi-step work, knowing when to stop |
| T4 | search memory, summarise | the memory-continuation path |
| T5 | count matches in a large file | long context, the CCR fold path |

T4 and T5 exist because both bugs found on 2026-09-22 lived there. A suite
of file-writing smoke tests would have passed while Spark returned empty
turns and a Codex subagent silently returned someone else's document.

T3's grader checks the test file is unmodified. "Make the tests pass" is
otherwise satisfiable by deleting the assertions.

## Verdicts

`completed` `wrong` `blank` `circling` `hung` `over_budget`

`blank` means thinking on the wire and no visible text — the literal
2026-09-22 Zen symptom, rendered by Claude Code as "Thought for 27s" and
nothing. `circling` means the same tool ran three or more times with
byte-identical arguments.

The artifact outranks the behavioural verdict: if the file on disk is
correct, the task passed.

## Liveness, and the mistake this encodes

Liveness is measured from transcript records and tool calls, never from turn
counters. A turn counter only moves between turns, so a model reasoning for
four minutes is indistinguishable from one that has died. Reading
`cursor_turn_received` as a liveness signal on 2026-09-22 produced a false
stall call on a pane that was working normally.

## The observer window

`run-bench.sh` opens one more window on a known-good Anthropic model,
holding `OBSERVER.md`. It reads the report, writes findings to
`findings.md`, and may re-run a single model and task to check whether a
failure reproduces.

It is told not to restart the proxy: other sessions run through it and a
restart drops their turns. It flags that a restart is needed and leaves it
to a human.

## Attribution caveat

Proxy faults are attributed by upstream model name and time window. If
another session drives the same model while a bench runs, its failures show
up here too. Keep runs clean, or read the proxy columns as an upper bound.

## Free tier

Spark runs on OpenCode Zen's free tier, where prompts may be used for
training. The fixtures are synthetic on purpose. Do not point bench tasks at
real repository content.
