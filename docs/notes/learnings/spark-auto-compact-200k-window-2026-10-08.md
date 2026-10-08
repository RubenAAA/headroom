# Client 2.1.288+ auto-compacts Spark at ~126k on an assumed 200k window; the DISABLE knob is dead

- **Status:** measured 2026-10-08. Source doc:
  `docs/notes/ideas/spark-usage-and-compaction.md` (Oct-5 addendum corrected
  the same day). The Oct-5 claim that
  `CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT=1` restores the old
  no-compact behavior is refuted — do not retry it without new evidence.
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS=1000000` is under test from 2026-10-08.
- **Window:** 2026-10-03 to 2026-10-08. Transcript census of 4,577 jsonl
  joined with /proc. Every count below is that window unless marked
  September.
- **Claim:** client 2.1.288+ treats Spark as `unknown-model` and enforces an
  assumed 200k window, auto-compacting at ~63% of it (~126k local tokens).
  The DISABLE knob does nothing.
- **Evidence (the cutover):** `muse-spark` has zero hits in client builds
  2.1.291–294, so Spark model ids stay unknown. 164 pure-Spark sessions on
  2.1.287 ran compaction-free, including 3,126-line sessions over 2.5 h. 132
  auto-compactions since Oct 3, all on 2.1.288+, preTokens 125,308–167,861.
  September's 34 compactions spread 189–557k — a different, looser regime.
  First of the new regime: 8 TapBuy sessions starting 2026-10-03T21:16Z.
- **Evidence (the knob is dead):** the DISABLE flag sat in the launcher and
  in every live claude process's environ, yet flagged top-level sessions
  still compacted at 125–137k. Post-flag cohort: 21 compacted vs 2 marginal
  non-compacted. It shipped Oct 5 with no verification; no evidence it ever
  worked.
- **Not the proxy:** usage reporting is unchanged (`input_tokens: 0` at
  message_start plus an output-only delta, `openai/stream.rs:1730–1790`).
  The stale-installer lead is rejected.
- **Watch:** the MAX_CONTEXT experiment moves the assumed window to 1M. If
  the ~63% trigger holds, compaction preTokens move from ~126k to ~630k.
  A long Spark session compacting near ~630k confirms; one still compacting
  near ~126k denies.
- **Resolved (2026-10-08 `[1m]` probe):** the two probe sessions were
  NOT Spark past turn 1 — `--continue` without `--model` fell back to
  the default model (transcript: `sys informational` "Session model
  muse-spark-1.3-contributor-free coul…" at 14:11:24, then every turn
  `claude-haiku-5-5`; turn 1 took 34–38s on Spark, turns 2+ took 7–12s
  on Haiku). Haiku 5-5 is known to the client (8 binary hits vs 0 for
  muse-spark), so its sessions honor `autoCompactWindow: 306000` and
  compacted at 293–295k ≈ 96% of it — no entrypoint effect. The
  126k-vs-294k gap is unknown-model vs known-model, and the user's
  "330k setting" was this setting all along. Corollary: the 5 Haiku
  recaches + c/r 0.169 spike in the same window are the probe's own
  exhaust (first recache: two identical control attempts colliding on
  one conversation key), not fleet behavior. `--model` with a `[1m]` id
  never reaches the wire (client strips it); the picker path is
  untested, and any redo must pass `--model` on every `-p` call.
  Details in the idea doc's Status.
