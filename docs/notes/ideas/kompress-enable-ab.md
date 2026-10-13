# Idea: enable Kompress ML prose compression and measure

- **Status:** open (never tried; live `--enable-kompress false`,
  `--force-kompress-all false`)
- **Source:** 2026-09-18 session; `contrib/headroom-flags.sh` ("OFF. … Untried:
  lossy compression sits close to instructions, and the live zone is the least
  valuable place to compress"); `config.rs:656-671,1695-1697`
- **Value:** ONNX prose model is already on disk
  (`~/.cache/huggingface/.../kompress-int8-wo.onnx`), so this is available,
  not theoretical. Only remaining lossy family never measured on live
  traffic — everything else lossy has numbers.
- **Since 2026-09-28:** `--disable-kompress` is live and overrides
  `--enable-kompress`, and the flags file sets it `true`, so the on-arm must
  flip both. `--force-kompress-all` is still a no-op in Rust; the forced arm
  needs it built first (route every non-excluded type to Kompress in
  `live_zone/dispatch.rs`, size gate kept).
- **2026-10-13, offline re-forward:** the installed binary is built
  without the `ml` feature, so the on-arm does nothing: startup logs
  `kompress_warm` with `kompress_ready=false` and no block takes
  Kompress. Bytes removed over the four captures: 652,048,667 off vs
  652,046,148 on. The model on disk is not enough; the A/B first needs
  a `--features ml` build and a libonnxruntime (`ORT_DYLIB_PATH`).
  Re-run with `reforward.py --set --enable-kompress=true --set
  --disable-kompress=false` against that build.
- **Also unlocks (2026-09-28):** the Responses `instructions` slot. Codex
  sends a 10–33KB prose system prompt on every turn (89/89 Codex turns,
  0/961 Spark in the 09-25 capture); no compressor takes it while Kompress
  is off. It is byte-identical across turns, so a deterministic Kompress
  pass would cache well. The slot is not wired on the Responses path yet —
  see `rejected/responses-instructions-call-input-slots.md` for the shape.
- **Next:** enable on a canary (NOT `--force-compress-all` yet — that
  bypasses guards; test guarded first, forced second if guarded wins).
  Track: per-turn savings on `PlainText` blocks; answer-retention quality
  (`text_crusher_quality_eval.py` shape — SQuAD retention + salient-token
  survival); drift waste (lossy-near-instructions is the stated worry, so
  watch system-adjacent blocks specifically); latency (model load/inference).
- **Exit:** keep on (possibly scoped to non-instruction-adjacent blocks) if
  savings land with no quality/drift regression; reject with the number
  otherwise.
