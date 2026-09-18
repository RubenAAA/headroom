#!/usr/bin/env python3
"""Do the regex and Magika content detectors disagree, and who compresses smaller?

    .venv/bin/python _detect_disagreement.py <capture_dir> [--limit N] [--min-bytes N]

Backend A (regex-only):  HEADROOM_DETECT_BACKEND=python -> _detect_content
Backend B (magika-first): HEADROOM_DETECT_BACKEND=rust  -> _detect_content
  (native magika chain plus the production guards: HTML/config overrides and
  the regex second opinion on PLAIN_TEXT — i.e. the exact candidate that
  wiring Magika into the Rust proxy would promote).

For every unique text block in the corpus client bodies:
  (a) agreement matrix regex x magika, with byte volumes;
  (b) on disagreements, ContentRouter.compress under each verdict
      (precomputed_detection) and compare compressed bytes;
  (c) per-backend detect latency.

Requires headroom importable (handled below: the parent of this directory
goes on sys.path, so run from bench/ with the repo .venv), headroom._core
built, and an ONNX Runtime lib for backend B: ORT_DYLIB_PATH or a pip
onnxruntime>=1.24 the loader discovers. Without it backend B degrades to
regex and the study is vacuous, so the script refuses to run when the
native preflight — or a canary classification — fails.
"""
import collections
import hashlib
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from cachesim import index_corpus, iter_client  # noqa: E402

import headroom.transforms.content_router as cr  # noqa: E402
from headroom.transforms.content_detector import ContentType, DetectionResult  # noqa: E402
from headroom.transforms.content_router import ContentRouter, ContentRouterConfig  # noqa: E402

CANARY_CODE = (
    "def hello():\n    print('world')\n\nclass Foo:\n    pass\n"
    "def bar(x):\n    return [i * 2 for i in range(x)]\n"
)


def detect(block, backend):
    """One verdict plus latency; tripped=True means the native watchdog fired
    and the verdict is regex-derived, not a real backend-B data point."""
    os.environ["HEADROOM_DETECT_BACKEND"] = backend
    if backend == "rust":
        cr._detect_native_unhealthy = False
    start = time.perf_counter()
    result = cr._detect_content(block)
    dt = time.perf_counter() - start
    tripped = backend == "rust" and cr._detect_native_unhealthy
    return result.content_type, dt, tripped


def iter_text_blocks(turns, min_bytes):
    """Yield unique text samples from client message bodies, largest first.

    Detection runs on tool outputs and prose, not on tool-use JSON, so only
    text and tool_result string parts are sampled. Order largest-first so
    --limit spends its budget where the bytes (and the bill) are.
    """
    seen = set()
    pending = []
    for turn in turns:
        try:
            messages = turn.body.get("messages", [])
        except AttributeError:
            continue
        for message in messages:
            content = message.get("content") if isinstance(message, dict) else None
            if isinstance(content, str):
                parts = [content]
            elif isinstance(content, list):
                parts = []
                for block in content:
                    if not isinstance(block, dict):
                        continue
                    if block.get("type") == "text" and isinstance(block.get("text"), str):
                        parts.append(block["text"])
                    elif block.get("type") == "tool_result":
                        inner = block.get("content")
                        if isinstance(inner, str):
                            parts.append(inner)
                        elif isinstance(inner, list):
                            parts.extend(
                                item["text"]
                                for item in inner
                                if isinstance(item, dict)
                                and item.get("type") == "text"
                                and isinstance(item.get("text"), str)
                            )
            else:
                continue
            for text in parts:
                size = len(text.encode("utf-8", errors="replace"))
                if size < min_bytes:
                    continue
                digest = hashlib.sha256(text.encode("utf-8", errors="replace")).hexdigest()
                if digest in seen:
                    continue
                seen.add(digest)
                pending.append((size, text))
    pending.sort(key=lambda item: -item[0])
    return [text for _, text in pending]


def main():
    args = sys.argv[1:]
    if not args or args[0] in ("-h", "--help"):
        print(__doc__)
        return 0
    corpus, limit, min_bytes = args[0], None, 200
    i = 1
    while i < len(args):
        if args[i] == "--limit":
            limit = int(args[i + 1])
            i += 2
        elif args[i] == "--min-bytes":
            min_bytes = int(args[i + 1])
            i += 2
        else:
            print(f"unknown argument: {args[i]}", file=sys.stderr)
            return 2

    from headroom._ort import rust_ort_runtime_compatible

    if not rust_ort_runtime_compatible():
        print(
            "refusing: native ORT preflight failed — backend B would degrade "
            "to regex and the study would be vacuous. Install onnxruntime>=1.24 "
            "or set ORT_DYLIB_PATH.",
            file=sys.stderr,
        )
        return 2
    canary, _, tripped = detect(CANARY_CODE, "rust")
    if tripped or canary is not ContentType.SOURCE_CODE:
        print(
            f"refusing: canary classified as {canary} (tripped={tripped}) — "
            "backend B is not really Magika in this process.",
            file=sys.stderr,
        )
        return 2

    blocks = iter_text_blocks(iter_client(index_corpus(corpus)), min_bytes)
    if limit is not None:
        blocks = blocks[:limit]
    total_bytes = sum(len(b.encode("utf-8", errors="replace")) for b in blocks)
    print(f"{len(blocks)} unique blocks >= {min_bytes}B, {total_bytes / 1e6:.1f} MB\n")

    matrix = collections.defaultdict(lambda: [0, 0])
    lat = {"python": [], "rust": []}
    agree_bytes = 0
    disagreed = []
    rust_failed = 0
    for n, block in enumerate(blocks, 1):
        size = len(block.encode("utf-8", errors="replace"))
        py_type, py_dt, _ = detect(block, "python")
        rs_type, rs_dt, tripped = detect(block, "rust")
        lat["python"].append(py_dt)
        lat["rust"].append(rs_dt)
        if tripped:
            rust_failed += 1
            continue
        matrix[(py_type, rs_type)][0] += 1
        matrix[(py_type, rs_type)][1] += size
        if py_type is rs_type:
            agree_bytes += size
        else:
            disagreed.append((block, size, py_type, rs_type))
        if n % 500 == 0:
            print(f"  detected {n}/{len(blocks)}...", flush=True)

    print(f"\nagreement: {agree_bytes / max(total_bytes, 1):.1%} of bytes "
          f"({len(blocks) - len(disagreed) - rust_failed}/{len(blocks)} blocks)")
    if rust_failed:
        print(f"backend-B watchdog trips (excluded): {rust_failed}")
    print("\ndisagreement pairs (regex -> magika): blocks, bytes:")
    for (py_type, rs_type), (count, size) in sorted(
        ((k, v) for k, v in matrix.items() if k[0] is not k[1]),
        key=lambda kv: -kv[1][1],
    ):
        print(f"  {py_type.value:>16} -> {rs_type.value:<16} {count:5d}  {size / 1e6:7.2f} MB")

    for backend, samples in lat.items():
        samples.sort()
        mean = sum(samples) / len(samples)
        print(f"detect latency {backend}: mean {mean * 1e3:.2f}ms, "
              f"max {samples[-1] * 1e3:.1f}ms over {len(samples)} calls")

    if not disagreed:
        print("\nno disagreements — nothing to compress.")
        return 0

    router = ContentRouter(ContentRouterConfig(enable_code_aware=True))
    print("router: enable_code_aware=True (mirrors the proxy's --code-aware true;\n"
          "the library default False would neuter every SOURCE_CODE verdict)")

    def compress_as(block, ctype, backend):
        # Pin the backend for the whole call: compress() re-detects inside
        # the MIXED splitter, and a stale env would run one arm's
        # sub-detection under the other backend (last time this produced
        # 141/141 ties and hid the entire effect).
        os.environ["HEADROOM_DETECT_BACKEND"] = backend
        det = DetectionResult(content_type=ctype, confidence=1.0, metadata={})
        out = router.compress(block, precomputed_detection=det)
        return len(out.compressed.encode("utf-8", errors="replace")), out.strategy_used

    py_win_bytes = rs_win_bytes = 0
    py_wins = rs_wins = ties = comp_fail = 0
    tie_same_strat = tie_diff_strat = 0
    examples = []
    for n, (block, size, py_type, rs_type) in enumerate(disagreed, 1):
        try:
            py_size, py_strat = compress_as(block, py_type, "python")
            rs_size, rs_strat = compress_as(block, rs_type, "rust")
        except Exception as exc:  # noqa: BLE001 — one bad block must not kill the run
            comp_fail += 1
            continue
        if py_size < rs_size:
            py_wins += 1
            py_win_bytes += rs_size - py_size
        elif rs_size < py_size:
            rs_wins += 1
            rs_win_bytes += py_size - rs_size
        else:
            ties += 1
            if py_strat is rs_strat:
                tie_same_strat += 1
            else:
                tie_diff_strat += 1
        examples.append((abs(py_size - rs_size), size, py_type, rs_type,
                         py_strat, rs_strat, block[:120].replace("\n", "\\n")))
        if n % 100 == 0:
            print(f"  compressed {n}/{len(disagreed)}...", flush=True)

    print(f"\ncompression on {len(disagreed)} disagreed blocks "
          f"({comp_fail} compressor failures excluded):")
    print(f"  regex verdict smaller:  {py_wins:5d} blocks, {py_win_bytes / 1e6:.2f} MB total edge")
    print(f"  magika verdict smaller: {rs_wins:5d} blocks, {rs_win_bytes / 1e6:.2f} MB total edge")
    print(f"  ties: {ties} (same strategy {tie_same_strat}, different strategy {tie_diff_strat})")
    print("\nlargest edges (edgeB, blockB, regex -> magika, strat_py -> strat_rs):")
    for edge, size, py_type, rs_type, py_strat, rs_strat, preview in sorted(
        examples, key=lambda e: -e[0]
    )[:10]:
        print(f"  {edge:8d} {size:8d}  {py_type.value} -> {rs_type.value}  "
              f"{py_strat.value} -> {rs_strat.value}  | {preview}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
