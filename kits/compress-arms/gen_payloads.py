#!/usr/bin/env python3
"""Synthetic-but-representative payload generator for compression-arm A/B.

Builds 20 Anthropic /v1/messages payloads (2-50KB) mixing:
  log    - timestamped log lines in Bash/BashOutput tool_result
  search - ripgrep-style matches in Bash tool_result (Grep tool is
           LosslessOnly, so search-shaped text rides Bash to be lossy-eligible)
  diff   - unified diff in Bash tool_result
  prose  - PlainText prose in human text + BashOutput tool_result (kompress arm)
  code   - source code in Bash tool_result (code-aware arm)
  json   - JSON blob (json_minifier)
  mixed  - multi-turn history with several tool_results
  excl   - excluded-tool controls (Read/Grep tool_result, must stay verbatim
           except the reversible lossless fold)

Usage: gen_payloads.py   (writes /tmp/compress-arms/payloads/pNN.json + manifest.json)
"""
import json
import random
from pathlib import Path

OUT = Path("/tmp/compress-arms/payloads")
OUT.mkdir(parents=True, exist_ok=True)
rng = random.Random(20261008)

TOOLS = [
    {"name": "Bash", "description": "Run shell commands",
     "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}},
    {"name": "Read", "description": "Read a file",
     "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}},
]

PROSE_PARAS = [
    "The quarterly review showed steady progress across all three workstreams, though the infrastructure migration continues to lag behind the original schedule by roughly two weeks.",
    "After considerable discussion the team agreed to postpone the storage backend decision until the benchmark results arrive, since committing early would lock in costs for the next fiscal year.",
    "One surprising outcome was the support burden: tickets dropped by a third after the documentation refresh, which nobody had predicted when the project started last spring.",
    "The on-call rotation remains the most contentious topic. Half the engineers want shorter shifts with more frequent handoffs, while the others argue that context loss during handoffs causes more incidents than fatigue.",
    "Budget constraints mean the contractor extension is unlikely to be approved, so the remaining integration work falls back on the core team, already stretched thin by the audit preparation.",
    "Performance testing revealed a regression in the upload path that traces back to the connection pooling change merged three weeks ago; reverting it restored the previous throughput immediately.",
    "Customer feedback on the new dashboard has been mixed. Power users praise the density of information, while newcomers report feeling overwhelmed by the number of panels on first load.",
    "The security review flagged two medium-severity findings in the session handling code, both of which have fixes staged and awaiting a second reviewer before the release branch cuts.",
]

LOG_TMPL = [
    "2026-10-08T{:02d}:{:02d}:{:02d}.{:03d}Z INFO  request_id={} method=POST path=/v1/messages status=200 latency_ms={} upstream=api.anthropic.com",
    "2026-10-08T{:02d}:{:02d}:{:02d}.{:03d}Z DEBUG cache_lookup key=prefix:{} hit={} age_s={}",
    "2026-10-08T{:02d}:{:02d}:{:02d}.{:03d}Z WARN  retry attempt={} backoff_ms={} reason=timeout upstream=api.anthropic.com",
    "2026-10-08T{:02d}:{:02d}:{:02d}.{:03d}Z ERROR request_id={} error=upstream_500 body_bytes={}",
    "2026-10-08T{:02d}:{:02d}:{:02d}.{:03d}Z INFO  compression mode={} tok_before={} tok_after={} opt_ms={}",
]

DIFF_FILES = ["crates/headroom-proxy/src/proxy.rs", "crates/headroom-core/src/transforms/live_zone.rs",
              "contrib/restart-headroom.sh", "docs/flags.md"]


def log_block(nbytes):
    lines = []
    i = 0
    while sum(len(l) + 1 for l in lines) < nbytes:
        t = LOG_TMPL[i % len(LOG_TMPL)]
        lines.append(t.format(
            rng.randrange(24), rng.randrange(60), rng.randrange(60),
            rng.randrange(1000), *random_args(t)))
        i += 1
    return "\n".join(lines)[:nbytes]


def random_args(t):
    out = []
    for _ in range(t.count("{}")):
        out.append(rng.choice([rng.randrange(10**8), rng.randrange(5000),
                               rng.randrange(10), "true" if rng.random() > 0.5 else "false",
                               "all_messages", "live_zone"]))
    return out


def search_block(nbytes):
    files = [f"src/{a}/{b}.rs" for a in ("proxy", "core", "handlers") for b in
             ("app", "state", "request_transforms", "replay", "continuation", "metrics")]
    lines = []
    while sum(len(l) + 1 for l in lines) < nbytes:
        f = rng.choice(files)
        lines.append(f"{f}:{rng.randrange(1,900)}:{rng.randrange(1,120)}: "
                     f"{rng.choice(['let compressed = dispatch(text, &arms);', 'tracing::info!(event = \"compression_ratio\", saved = %saved);', 'fn compress_one_block(block: &Block, ctx: &Ctx) -> Outcome {', 'if tok_after < tok_before * threshold {{ return Passthrough; }}', 'let outcome = arms.iter().fold(input, |acc, arm| arm.apply(acc));'])}")
    return "\n".join(lines)[:nbytes]


def diff_block(nbytes):
    chunks = []
    while sum(len(c) + 1 for c in chunks) < nbytes:
        f = rng.choice(DIFF_FILES)
        start = rng.randrange(1, 800)
        body = [f"diff --git a/{f} b/{f}", "index 1a2b3c4..5d6e7f8 100644",
                f"--- a/{f}", f"+++ b/{f}", f"@@ -{start},7 +{start},9 @@"]
        for _ in range(7):
            r = rng.random()
            line = rng.choice(["    let before = body.len();", "    let after = compressed.len();",
                               "    tracing::debug!(saved = before - after);", "    Ok(outcome)",
                               "    let threshold = ctx.min_savings();", "    if text.is_empty() { return input; }",
                               "    let strategy = detect(content_type);"])
            body.append((" " if r > 0.4 else ("-" if r > 0.2 else "+")) + line)
        chunks.append("\n".join(body))
    return "\n".join(chunks)[:nbytes]


def prose_block(nbytes):
    paras = []
    while sum(len(p) + 2 for p in paras) < nbytes:
        paras.append(rng.choice(PROSE_PARAS))
    return "\n\n".join(paras)[:nbytes]


REAL_CODE = (Path("/home/ruben/headroom/crates/headroom-core/src/transforms/"
                "dense_line_elider.rs").read_text() +
             Path("/home/ruben/headroom/crates/headroom-core/src/tool_exclusion.rs")
             .read_text())
REAL_PROSE = Path("/home/ruben/headroom/AGENTS.md").read_text()


def code_block(nbytes):
    # Real source: random-token soup classifies as PlainText and engages
    # nothing; real code exercises the CodeCompressor arm.
    rep = (REAL_CODE + "\n") * (nbytes // len(REAL_CODE) + 1)
    return rep[:nbytes]


def real_prose_block(nbytes):
    rep = (REAL_PROSE + "\n") * (nbytes // len(REAL_PROSE) + 1)
    return rep[:nbytes]


def json_block(nbytes):
    obj = {"requests": [
        {"id": i, "path": "/v1/messages", "model": "claude-opus-4-6",
         "usage": {"input_tokens": rng.randrange(50000), "output_tokens": rng.randrange(4000)},
         "flags": {"compression": True, "mode": "all_messages", "lossless": False}}
        for i in range(max(2, nbytes // 220))]}
    return json.dumps(obj, indent=2)[:nbytes]


def tool_result(tool_use_id, text):
    # STRING content shape: the live-zone dispatcher (B3) only compresses
    # string tool_result.content; structured-array content is a no-op.
    return {"type": "tool_result", "tool_use_id": tool_use_id,
            "content": text}


def tool_use_block(tid, name, cmd):
    return {"type": "tool_use", "id": tid, "name": name, "input": {"command": cmd}}


def payload(messages):
    return {"model": "claude-opus-4-6", "max_tokens": 1024, "stream": False,
            "messages": messages, "tools": TOOLS}


SPECS = [
    # (name, builder, target_bytes)
    ("log-small", lambda: payload([
        {"role": "user", "content": [
            {"type": "text", "text": "Here is the tail of the proxy log. Any errors?"},
            tool_result("tu_log1", log_block(1800))]}]), 2200),
    ("log-medium", lambda: payload([
        {"role": "user", "content": [
            {"type": "text", "text": "Full proxy log from the incident window:"},
            tool_result("tu_log2", log_block(9500))]}]), 10000),
    ("log-large", lambda: payload([
        {"role": "user", "content": [
            {"type": "text", "text": "Overnight log dump for analysis:"},
            tool_result("tu_log3", log_block(28000))]}]), 29000),
    ("search-small", lambda: payload([
        {"role": "assistant", "content": [tool_use_block("tu_s1", "Bash", "rg -n compress_one_block src")]},
        {"role": "user", "content": [tool_result("tu_s1", search_block(2600))]}]), 3000),
    ("search-medium", lambda: payload([
        {"role": "assistant", "content": [tool_use_block("tu_s2", "Bash", "rg -n 'tok_after|tokens_after' crates")]},
        {"role": "user", "content": [tool_result("tu_s2", search_block(12000))]}]), 12500),
    ("diff-small", lambda: payload([
        {"role": "user", "content": [
            {"type": "text", "text": "Review this diff:"},
            tool_result("tu_d1", diff_block(3200))]}]), 3600),
    ("diff-medium", lambda: payload([
        {"role": "assistant", "content": [tool_use_block("tu_d2", "Bash", "git diff HEAD~3 --stat; git diff HEAD~3")]},
        {"role": "user", "content": [tool_result("tu_d2", diff_block(15000))]}]), 15500),
    ("diff-large", lambda: payload([
        {"role": "user", "content": [
            {"type": "text", "text": "Release diff for review:"},
            tool_result("tu_d3", diff_block(45000))]}]), 46000),
    ("prose-small", lambda: payload([
        {"role": "user", "content": [
            {"type": "text", "text": "Summarize this status update:\n\n" + prose_block(1900)}]}]), 2300),
    ("prose-medium", lambda: payload([
        {"role": "user", "content": [
            {"type": "text", "text": "Meeting notes to condense:\n\n" + prose_block(8500)},
            tool_result("tu_p1", "notes.txt contents:\n" + prose_block(4000))]}]), 13000),
    ("prose-large", lambda: payload([
        {"role": "assistant", "content": [tool_use_block("tu_p2", "Bash", "cat reports/q3-summary.md")]},
        {"role": "user", "content": [tool_result("tu_p2", real_prose_block(32000))]}]), 32500),
    ("code-medium", lambda: payload([
        {"role": "assistant", "content": [tool_use_block("tu_c1", "Bash", "sed -n '1,400p' crates/headroom-core/src/transforms/live_zone.rs")]},
        {"role": "user", "content": [tool_result("tu_c1", code_block(11000))]}]), 11500),
    ("code-large", lambda: payload([
        {"role": "user", "content": [
            {"type": "text", "text": "Audit this module:"},
            tool_result("tu_c2", code_block(38000))]}]), 38500),
    ("json-medium", lambda: payload([
        {"role": "assistant", "content": [tool_use_block("tu_j1", "Bash", "curl -s localhost:8787/stats | head -c 9000")]},
        {"role": "user", "content": [tool_result("tu_j1", json_block(9000))]}]), 9500),
    ("mixed-turns", lambda: payload([
        {"role": "user", "content": [
            {"type": "text", "text": "Investigate the latency spike."},
            tool_result("tu_m1", log_block(5000))]},
        {"role": "assistant", "content": [
            {"type": "text", "text": "I see elevated p99. Pulling the hot path now."},
            tool_use_block("tu_m2", "Bash", "rg -n 'fn hot_path' src")]},
        {"role": "user", "content": [
            tool_result("tu_m2", search_block(4000)),
            {"type": "text", "text": prose_block(3000)}]},
        {"role": "assistant", "content": [
            {"type": "text", "text": "Found it. Checking the recent diff."},
            tool_use_block("tu_m3", "Bash", "git diff HEAD~1")]},
        {"role": "user", "content": [tool_result("tu_m3", diff_block(6000))]}]), 19000),
    ("mixed-small", lambda: payload([
        {"role": "user", "content": [
            {"type": "text", "text": "Quick check:"},
            tool_result("tu_ms1", log_block(1200))]},
        {"role": "assistant", "content": [
            {"type": "text", "text": "Looks fine. Anything else?"},
            tool_use_block("tu_ms2", "Bash", "git status --short")]},
        {"role": "user", "content": [
            tool_result("tu_ms2", " M crates/headroom-proxy/src/proxy.rs\n M docs/flags.md\n"),
            {"type": "text", "text": prose_block(900)}]}]), 3200),
    ("excl-read-code", lambda: payload([
        {"role": "assistant", "content": [{"type": "tool_use", "id": "tu_e1", "name": "Read",
                                           "input": {"path": "crates/headroom-core/src/transforms/live_zone.rs"}}]},
        {"role": "user", "content": [dict(tool_result("tu_e1", code_block(14000)),
                                          **{"_tool_name_hint": "Read"})]}]), 14500),
    ("excl-grep-search", lambda: payload([
        {"role": "assistant", "content": [{"type": "tool_use", "id": "tu_e2", "name": "Grep",
                                           "input": {"pattern": "tok_after"}}]},
        {"role": "user", "content": [tool_result("tu_e2", search_block(7000))]}]), 7500),
    ("tiny-prose", lambda: payload([
        {"role": "user", "content": [{"type": "text", "text": prose_block(1500)}]}]), 1900),
    ("tiny-log", lambda: payload([
        {"role": "user", "content": [tool_result("tu_t1", log_block(1700))]}]), 2100),
]

# NOTE: tool_result blocks carry no tool name in the Anthropic wire format;
# the proxy recovers it from the matching assistant tool_use id. The
# excl-read-code payload pairs its result with a Read tool_use for that link.
# (_tool_name_hint is stripped before writing; it only documents intent.)


def main():
    manifest = []
    for i, (name, build, target) in enumerate(SPECS, 1):
        obj = build()
        # Auto-pair: every tool_result needs a matching assistant tool_use,
        # else the proxy orphan-repairs it to user text, which the Balanced
        # regime skips — and nothing compresses. Insert the assistant turn
        # before the user message holding the orphan result.
        used = set()
        for m in obj["messages"]:
            for b in (m.get("content") or []):
                if isinstance(b, dict) and b.get("type") == "tool_use":
                    used.add(b["id"])
        fixed = []
        for m in obj["messages"]:
            orphans = [b for b in (m.get("content") or [])
                       if isinstance(b, dict) and b.get("type") == "tool_result"
                       and b.get("tool_use_id") not in used]
            for b in orphans:
                hint = b.get("_tool_name_hint", "Bash")
                tid = b["tool_use_id"]
                fixed.append({"role": "assistant", "content": [
                    {"type": "text", "text": "Running %s." % hint},
                    {"type": "tool_use", "id": tid, "name": hint,
                     "input": {"command": "synthetic"}}]})
                used.add(tid)
            fixed.append(m)
        obj["messages"] = fixed

        def strip(o):
            if isinstance(o, dict):
                o.pop("_tool_name_hint", None)
                for v in o.values():
                    strip(v)
            elif isinstance(o, list):
                for v in o:
                    strip(v)
        strip(obj)
        raw = json.dumps(obj).encode()
        (OUT / f"p{i:02d}-{name}.json").write_bytes(raw)
        manifest.append({"file": f"p{i:02d}-{name}.json", "shape": name,
                         "target_bytes": target, "actual_bytes": len(raw)})
    (OUT / "manifest.json").write_text(json.dumps(manifest, indent=1))
    total = sum(m["actual_bytes"] for m in manifest)
    print(f"wrote {len(manifest)} payloads, {total} bytes total")
    for m in manifest:
        print(f"  {m['file']}: {m['actual_bytes']}B")


if __name__ == "__main__":
    main()
