# Investigation kits

Side-proxy suites and measurement scripts behind the findings in
`docs/notes/ideas/` and `docs/notes/learnings/`. Each kit reruns against a
side proxy on the installed binary — never `:8787`. Bulky regenerable
outputs (captures, proxy state, logs) stay local per `.gitignore`; scripts,
manifests and small reports are tracked.

- `msg0/` — messages[0]-drop follow-ups (Oct 8). Connect-level contention
  + length/hash audits: `run-side.sh`; response-side truncation matrix:
  `./run-trunc.sh b1 b2 b3 b4 b5` (side ports `:8791`/`:18883`).
  Verdict: not reproduced on any check; closed as non-proxy.
- `probe-1m/` — `[1m]` compaction-window probe (Oct 8).
  `drive.sh <arm-dir> <model> <start-turn> <end-turn>` grows a `-p`
  session turn by turn. Verdict: client strips `[1m]` on `--model`
  AND the picker path; `--continue` without `--model` falls back to
  Haiku — rerun must pass `--model` on every invocation.
- `egate-sideproxy/` — E-gate A/B (`run-test.sh`/`control.sh`, `send.py`,
  `soak.py`, `grow.py`, `edge.py`); `captures-prev/` + `gen2-evidence/`
  hold the runs behind the E1–E4 verdicts.
- `compress-arms/` — compression-arm A/B (`side_proxy.sh`, payloads in
  `payloads/`, analysis in `run/` + `analyze.py`).
- `jev-refresh/` — Jev observer calibration (`CMDS.txt`, `numbers.txt`).
- `ccr-spark-measure/` — CCR-on-Spark aftershock measurement.
- `shadow-rate/` — shadow-tool-call rate (`COMMANDS.txt`).
- `drops-audit/` — Spark request-shape audit (`08_final.py` + outputs).
