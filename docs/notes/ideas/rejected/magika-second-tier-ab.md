# Idea: Magika as second-tier detection (PlainText recapture), measured offline first

- **Status:** REJECTED 2026-09-18 — measured, net zero-to-negative (see below).
  Harness kept at `upstream-python/bench/_detect_disagreement.py`.
- **Source:** 2026-09-18 session; `transforms/detection.rs:59` (magika
  chain: Tier 1 magika → Tier 2 unidiff → Tier 3 PlainText) vs
  `transforms/content_router.rs:1431` (`detect_content_native` uses the
  regex `content_detector`, which is what actually runs live)
- **Value:** the regex detector already covers code/JSON/HTML/diff with
  tested detections, so Magika-as-replacement can only lose — but Magika
  as a tier *behind* the regex could recapture content the regex calls
  PlainText (missed code/JSON/HTML) into CodeAware/SmartCrusher instead
  of passthrough (Kompress is off, so PlainText currently passes through
  untouched). NOT a replacement: the magika chain yields 5 types and by
  locked design sends grep/search and build/log output to PlainText
  (`detection.rs:39-47`), which would un-compress the Search/Log routes
  the regex owns — exactly the tool-result traffic the bill is made of.
- **Next:** offline disagreement harness over captured bodies (no live
  traffic, no proxy wiring): (a) disagreement rate regex vs magika chain;
  (b) on disagreements, which routing compresses smaller (bytes +
  strategy outcome); (c) per-block inference latency (singleton `Mutex`
  session). Needs the ORT lib on the bench machine only
  (`pip install onnxruntime` in a scanned venv, or `ORT_DYLIB_PATH`) —
  the proxy needs nothing until a wire decision. Box has AVX2 (Ultra 9
  285H), so Magika can run here once the lib exists.
- **Exit:** wire as second tier (regex first, Magika recaptures PlainText
  only) if recaptured volume is material with no latency blowup; reject
  with the number otherwise. Replacement is off the table regardless —
  losing Search/Log routing is a known regression, not a hypothesis.
- **Measured 2026-09-18** (`_detect_disagreement.py`, regex-only vs
  magika-first-with-guards over captured client bodies, unique text blocks
  >= 200B, `ContentRouter.compress` under each verdict with
  `enable_code_aware=True` to mirror `--code-aware true`; ORT 1.28.0 in
  `.venv`, `ORT_DYLIB_PATH` set):
  - toolblocks (806 blocks, 2.0 MB): 86.8% byte agreement; 141
    disagreements → 140 ties, 1 magika win of 379B.
  - drift (268 blocks): 66.7% byte agreement; 93 disagreements → 90
    ties, 3 REGEX wins totalling ~5.2KB (magika routed build logs with
    embedded code to SOURCE_CODE; LOG beat CODE_AWARE by 1.4–2.0KB each).
  - Net across 1,074 blocks: ~−5KB on ~2.5MB. Disagreement is real
    (13–33% of bytes) but does not convert: code-aware's re-parse gate
    declines to original on most recaptured code, and mixed blocks
    converge in the splitter.
  - Latency: magika ~13–15ms vs regex ~0.5ms per block (~30x), before any
    ORT install, slow-build-dep retention, or flip-time cache churn.
  - Harness notes for re-runs: pin `HEADROOM_DETECT_BACKEND` per arm
    around `compress()` (the MIXED splitter re-detects internally; a
    stale env measured 141/141 false ties), and keep
    `enable_code_aware=True` (the library default False neuters every
    SOURCE_CODE verdict).
