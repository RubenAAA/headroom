#!/usr/bin/env python3
"""Would candidate regex rules fix Magika-caught misses without regressions?

    .venv/bin/python _detect_miss_probes.py <capture_dir> [--min-bytes N]

Method: exact full-pipeline simulation. The candidate rules (extra code
patterns, `N<TAB>`/`N │` code-view tolerance, \\t line-number guard) are
installed into the production Python detector at runtime (monkeypatch —
files untouched) and the orchestrator order/gates are replicated; a
self-check first proves the replica reproduces the stock verdicts exactly.
Then: fixes on the miss set (regex text/tabular where magika sees
source_code|json_array|diff), regressions on the agreement set (any
verdict change where both detectors agreed).
"""
import hashlib
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from cachesim import index_corpus, iter_client  # noqa: E402

import headroom.transforms.content_router as cr  # noqa: E402
import headroom.transforms.content_detector as cd  # noqa: E402
from headroom.transforms.content_detector import (  # noqa: E402
    ContentType,
    detect_content_type as regex_detect,
)

os.environ["HEADROOM_DETECT_BACKEND"] = "rust"

# ── Candidate v2 rules ──────────────────────────────────────────────
#
# Evaluated as isolated families (one run each) so a poisonous rule can't
# hide behind good ones. The shell rule is conjunctive — shebang in the
# first 3 non-empty lines AND >=2 shell-ish lines — because a bare `WORD=`
# pattern fires on analysis prose (`dob=both`, `name_sim=1.0`).

V2_FAMILIES = {
    "go-assign": {
        "patterns": {"go": [re.compile(r"^\s*\w[\w.]*\s*:=")]},
        "code_view": False, "tab_guard": False, "shell_conj": False,
    },
    "sql": {
        "patterns": {
            "sql": [
                re.compile(r"^\s*(SELECT|WITH|INSERT\s+INTO|UPDATE|DELETE\s+FROM)\b"),
                re.compile(r"^\s*(FROM|WHERE|JOIN|GROUP\s+BY|ORDER\s+BY|HAVING|LIMIT)\b"),
            ]
        },
        "code_view": False, "tab_guard": False, "shell_conj": False,
    },
    "shell-conj": {
        "patterns": {},
        "code_view": False, "tab_guard": False, "shell_conj": True,
    },
    "code-view": {
        "patterns": {},
        "code_view": True, "tab_guard": False, "shell_conj": False,
    },
    "tab-guard": {
        "patterns": {},
        "code_view": False, "tab_guard": True, "shell_conj": False,
    },
    "combined": {
        "patterns": "ALL",
        "code_view": True, "tab_guard": True, "shell_conj": True,
    },
}

SHELL_BODY = re.compile(
    r"^\s*[A-Za-z_]\w*=\S"          # PW=$(...) / PGPASSWORD=...
    r"|^\s*(cd|export|exit|exec|source)\b"
    r"|\$\("                          # $(...) anywhere
    r"|^\s*(if|then|fi|for|while|do|done|case|esac|function)\b"
)


def shell_conjunctive(text):
    """Shebang within the first 3 non-empty lines (tolerates a tool-output
    header like `Exit code 1`) plus >=2 shell-ish body lines."""
    nonempty = [ln for ln in text.splitlines() if ln.strip()][:100]
    if not any(ln.startswith("#!") for ln in nonempty[:3]):
        return False
    return sum(1 for ln in nonempty if SHELL_BODY.search(ln)) >= 2

CODE_VIEW_TAB = re.compile(r"^\s*\d+\t")
CODE_VIEW_BAR = re.compile(r"^\s*\d+\s*[│|]\s?")


def code_view(line):
    """Detection-time view for CODE patterns only: tolerate `N<TAB>` and
    `N │` line-number prefixes. Tabular/search/log see the raw line."""
    line = CODE_VIEW_TAB.sub("", line)
    line = CODE_VIEW_BAR.sub("", line)
    return line


def tab_guard_declines(text):
    """True if the \\t-delimiter candidacy should be skipped: sampled rows'
    first fields are strictly increasing integers (line numbers, not data).
    Other delimiters (`,`/`;` carry real id-column CSVs) are untouched."""
    lines = [ln for ln in text.splitlines() if ln.strip()][:50]
    if len(lines) < 3:
        return False
    firsts = []
    for row in lines[:20]:
        if "\t" not in row:
            return False
        first = row.split("\t", 1)[0].strip()
        if not re.fullmatch(r"\d+", first):
            return False
        firsts.append(int(first))
    return all(b > a for a, b in zip(firsts, firsts[1:]))


def sim_verdict(text, fam):
    """Replicate detect_content_type order/gates; fam selects which v2
    family is active (None = stock replica for the self-check)."""
    use_view = fam is not None and fam["code_view"]
    use_guard = fam is not None and fam["tab_guard"]
    r = cd._try_detect_json(text)
    if r:
        return r.content_type
    r = cd._try_detect_diff(text)
    if r and r.confidence >= 0.7:
        return r.content_type
    r = cd._try_detect_html(text)
    if r and r.confidence >= 0.7:
        return r.content_type
    r = cd._try_detect_search(text)
    if r and r.confidence >= 0.6:
        return r.content_type
    r = cd._try_detect_log(text)
    if r and r.confidence >= 0.5:
        return r.content_type
    if not (use_guard and tab_guard_declines(text)):
        r = cd._try_detect_tabular(text)
        if r and r.confidence >= 0.6:
            return r.content_type
    r = cd._try_detect_structured_config(text)
    if r and r.confidence >= 0.6:
        return r.content_type
    if fam is not None and fam["shell_conj"] and shell_conjunctive(text):
        return ContentType.SOURCE_CODE
    code_in = "\n".join(code_view(l) for l in text.split("\n")) if use_view else text
    r = cd._try_detect_code(code_in)
    if r and r.confidence >= 0.5:
        return r.content_type
    return ContentType.PLAIN_TEXT


def iter_blocks(corpus, min_bytes):
    seen = set()
    for turn in iter_client(index_corpus(corpus)):
        for message in turn.body.get("messages", []):
            content = message.get("content") if isinstance(message, dict) else None
            texts = [content] if isinstance(content, str) else []
            if isinstance(content, list):
                for block in content:
                    if not isinstance(block, dict):
                        continue
                    if block.get("type") == "text" and isinstance(block.get("text"), str):
                        texts.append(block["text"])
                    elif block.get("type") == "tool_result":
                        inner = block.get("content")
                        if isinstance(inner, str):
                            texts.append(inner)
            for text in texts:
                if len(text.encode("utf-8", errors="replace")) < min_bytes:
                    continue
                digest = hashlib.sha256(text.encode("utf-8", errors="replace")).hexdigest()
                if digest not in seen:
                    seen.add(digest)
                    yield text


def main():
    args = sys.argv[1:]
    if not args or args[0] in ("-h", "--help"):
        print(__doc__)
        return 0
    corpus, min_bytes = args[0], 200
    if "--min-bytes" in args:
        min_bytes = int(args[args.index("--min-bytes") + 1])

    from headroom._ort import rust_ort_runtime_compatible

    assert rust_ort_runtime_compatible(), "need ORT for the magika side"

    blocks = list(iter_blocks(corpus, min_bytes))

    # Self-check: replica without v2 must equal stock verdicts exactly.
    stock = [regex_detect(t).content_type for t in blocks]
    replica = [sim_verdict(t, None) for t in blocks]
    drift = sum(1 for a, b in zip(stock, replica) if a is not b)
    print(f"self-check: replica vs stock differ on {drift}/{len(blocks)} blocks")
    assert drift == 0, "orchestrator replica is unfaithful — fix before measuring"

    magika = []
    for text in blocks:
        cr._detect_native_unhealthy = False
        magika.append(cr._detect_content(text).content_type)

    misses, agreements = [], []
    for text, py, rs in zip(blocks, stock, magika):
        if py in (ContentType.PLAIN_TEXT, ContentType.TABULAR) and rs in (
            ContentType.SOURCE_CODE,
            ContentType.JSON_ARRAY,
            ContentType.GIT_DIFF,
        ):
            misses.append((text, py, rs))
        elif py is rs:
            agreements.append((text, py))
    print(f"{len(blocks)} blocks: {len(misses)} regex-misses, "
          f"{len(agreements)} agreements\n")

    pristine = {lang: list(pats) for lang, pats in cd._CODE_PATTERNS.items()}
    # The union of every family's added patterns, for the combined run.
    all_added = {}
    for fam in V2_FAMILIES.values():
        if isinstance(fam["patterns"], dict):
            for lang, patterns in fam["patterns"].items():
                all_added.setdefault(lang, []).extend(patterns)
    for fname, fam in V2_FAMILIES.items():
        cd._CODE_PATTERNS.clear()
        cd._CODE_PATTERNS.update({lang: list(pats) for lang, pats in pristine.items()})
        added = all_added if fam["patterns"] == "ALL" else fam["patterns"]
        for lang, patterns in added.items():
            cd._CODE_PATTERNS.setdefault(lang, []).extend(patterns)

        fixed = 0
        by_pair = {}
        for text, py, rs in misses:
            if sim_verdict(text, fam) is rs:
                fixed += 1
                by_pair[(py.value, rs.value)] = by_pair.get((py.value, rs.value), 0) + 1
        print(f"[{fname}] fixes {fixed}/{len(misses)} misses")
        for pair, count in sorted(by_pair.items(), key=lambda kv: -kv[1]):
            print(f"    {pair[0]} -> {pair[1]}: {count}")

        moved = 0
        examples = []
        for text, py in agreements:
            new = sim_verdict(text, fam)
            if new is not py:
                moved += 1
                if len(examples) < 40:
                    examples.append((text, py.value, new.value))
        print(f"[{fname}] moves {moved}/{len(agreements)} agreements")
        for text, old, new in examples:
            print(f"    {old} -> {new} | {text[:100].replace(chr(10), '\\\\n')}")
            shown_lines = 0
            for i, line in enumerate(text.split("\n")[:100]):
                probe_line = code_view(line) if fam["code_view"] else line
                hits = []
                for lang, patterns in cd._CODE_PATTERNS.items():
                    for p in patterns:
                        if p.match(probe_line):
                            hits.append(f"{lang}:{p.pattern[:28]}")
                            break
                if hits:
                    print(f"        L{i} [{'; '.join(hits)}] {line[:70]}")
                    shown_lines += 1
                    if shown_lines >= 6:
                        break
        print()
    cd._CODE_PATTERNS.clear()
    cd._CODE_PATTERNS.update(pristine)
    return 0


if __name__ == "__main__":
    sys.exit(main())
