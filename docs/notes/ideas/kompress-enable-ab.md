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
- **Next:** enable on a canary (NOT `--force-compress-all` yet — that
  bypasses guards; test guarded first, forced second if guarded wins).
  Track: per-turn savings on `PlainText` blocks; answer-retention quality
  (`text_crusher_quality_eval.py` shape — SQuAD retention + salient-token
  survival); drift waste (lossy-near-instructions is the stated worry, so
  watch system-adjacent blocks specifically); latency (model load/inference).
- **Exit:** keep on (possibly scoped to non-instruction-adjacent blocks) if
  savings land with no quality/drift regression; reject with the number
  otherwise.
