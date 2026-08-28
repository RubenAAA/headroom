#!/usr/bin/env python3
"""Measure the proxy against the client it proxies, on real billing.

    python3 ab_replay.py [--runs 3] [--turns 6] [--model sonnet] [--keep]

This is the experiment that settles "is the proxy cheaper than plain Claude Code".
It reads the real bill, not a model of it: every `claude -p` invocation reports
`modelUsage` with the provider's own read / create / fresh counts.

# Why it is shaped like this

A raw replay of captured request bodies would be perfectly deterministic, and it
is not available: Anthropic answers a subscription OAuth token with HTTP 429
unless the caller is its own client, whatever headers you send. So Claude Code has
to be the client, and the job is to remove its freedom to vary.

- **No tools.** `--disallowedTools` covers every tool the client offers, so each
  prompt produces exactly one assistant turn. Turn count was the largest source
  of variance in the first attempt at this: 9 turns against 10 and 11.
- **Scripted turns.** Each user message is fixed text supplied by this script and
  replayed with `--resume`, so both arms send the same conversation.
- **A fixed bulk payload** in the opening message gives the prompt enough size for
  caching to matter without a tool call to fetch it.
- **Both arms behind a proxy.** Pointed straight at api.anthropic.com the client
  grants itself a 1,000,000-token context window; behind any other base URL it
  uses 200,000. A different context window is a different client, so the base arm
  runs through a transparent proxy instance this script starts and stops.
- **A nonce per run.** Two identical runs share provider cache entries and the
  second reads what the first paid for. That inverted the verdict once already:
  the base arm created 87,039 tokens on its first run and 51,860 on its second.
- **Hooks off.** `--settings '{"hooks":{}}'` keeps SessionStart hooks out,
  including the one that swaps credentials and would re-key a conversation
  mid-run.

# Reading the output

`fitted` is the bill in fresh-token equivalents under the weights
`fit_weights.py fit` recovered from Anthropic's own meter: fresh 1.0, 1h write
1.45, read 0.09. Reads are NOT free — "reads free" fits the meter worse than the
published weights — so a change that trades writes for reads has to trade about
sixteen of them to break even.

Watch `fresh` as closely as the total. Anything after the last `cache_control`
marker bills at full price, and a marker placed earlier than the client's own
forfeits coverage the client had; that alone was 4.4 points of an 11.5% loss on
2026-08-17. `bench/coverage.py` catches the same fault statically, for free, and
should be run first.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import uuid

PROXY_URL = "http://127.0.0.1:8787"
BASE_PORT = 8799
BASE_URL = f"http://127.0.0.1:{BASE_PORT}"
CONFIG_DIR = os.environ.get("CLAUDE_CONFIG_DIR", os.path.expanduser("~/.claude"))
PROXY_BIN = os.path.join(os.path.dirname(__file__), "..", "target", "release", "headroom-proxy")

# Every tool the client might offer. Naming them all leaves the model with no
# tool to call, which is what pins one assistant turn per prompt.
NO_TOOLS = [
    "Bash", "Read", "Write", "Edit", "Glob", "Grep", "Task", "WebFetch",
    "WebSearch", "NotebookEdit", "TodoWrite", "Agent", "LSP", "Monitor",
    "SendMessage", "Skill", "TaskOutput", "TaskStop", "EnterWorktree",
    "ExitWorktree", "ListAgents",
]

WEIGHTS = {"fresh": 1.0, "write": 1.45, "read": 0.09}


def bulk_payload(path: str, target_bytes: int) -> str:
    """Deterministic filler so the prompt is big enough for caching to matter."""
    with open(path, errors="replace") as handle:
        text = handle.read()
    while len(text) < target_bytes:
        text += text
    return text[:target_bytes]


def start_base_proxy(log_path: str, store: str) -> subprocess.Popen | None:
    """A deliberately transparent proxy, so the base arm is the bare client."""
    args = [
        os.path.abspath(PROXY_BIN),
        "--listen", f"127.0.0.1:{BASE_PORT}",
        "--upstream", "https://api.anthropic.com",
        "--compression-mode", "off",
        "--prefix-replay", "false",
        "--memory", "false",
        "--ctx-capture", "false",
        "--ctx-offload", "false",
        "--ctx-inject", "false",
        "--cache-control-auto-frozen", "disabled",
        "--strip-system-cache-breakpoints", "false",
        "--force-1h-cache-ttl", "false",
        "--hold-working-directory", "false",
        "--ctx-store-dir", store,
    ]
    handle = open(log_path, "wb")
    proc = subprocess.Popen(args, stdout=handle, stderr=handle, stdin=subprocess.DEVNULL)
    for _ in range(80):
        if subprocess.run(["ss", "-ltn", f"sport = :{BASE_PORT}"],
                          capture_output=True, text=True).stdout.count(str(BASE_PORT)):
            return proc
        if proc.poll() is not None:
            return None
        time.sleep(0.25)
    return None


def run_conversation(base_url: str, prompts: list[str], model: str) -> dict:
    """Run one scripted conversation, returning summed provider usage."""
    session = str(uuid.uuid4())
    env = dict(os.environ, ANTHROPIC_BASE_URL=base_url, CLAUDE_CONFIG_DIR=CONFIG_DIR)
    total = {"fresh": 0, "write": 0, "read": 0, "turns": 0, "usd": 0.0}
    for i, prompt in enumerate(prompts):
        cmd = [
            "claude", "-p", prompt, "--model", model,
            "--max-turns", "2", "--settings", '{"hooks":{}}',
            "--output-format", "json", "--disallowed-tools", *NO_TOOLS,
        ]
        cmd += ["--session-id", session] if i == 0 else ["--resume", session]
        done = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=600)
        if done.returncode != 0:
            print(f"    turn {i + 1} failed: {done.stderr.strip()[:160]}", file=sys.stderr)
            return total
        try:
            payload = json.loads(done.stdout)
        except ValueError:
            print(f"    turn {i + 1}: unparseable result", file=sys.stderr)
            return total
        usage = list((payload.get("modelUsage") or {}).values())
        if not usage:
            continue
        u = usage[0]
        total["fresh"] += u.get("inputTokens", 0)
        total["write"] += u.get("cacheCreationInputTokens", 0)
        total["read"] += u.get("cacheReadInputTokens", 0)
        total["usd"] += u.get("costUSD", 0.0)
        total["turns"] += payload.get("num_turns", 0)
    return total


def fitted(row: dict) -> float:
    return (row["fresh"] * WEIGHTS["fresh"]
            + row["write"] * WEIGHTS["write"]
            + row["read"] * WEIGHTS["read"])


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--turns", type=int, default=6)
    ap.add_argument("--model", default="sonnet")
    ap.add_argument("--payload-bytes", type=int, default=60_000)
    ap.add_argument("--payload-file", default=os.path.join(os.path.dirname(__file__), "cachesim.py"))
    ap.add_argument("--keep", action="store_true", help="leave the base proxy running")
    args = ap.parse_args()

    store = tempfile.mkdtemp(prefix="ab_replay_")
    log_path = os.path.join(store, "base-proxy.log")
    proc = start_base_proxy(log_path, os.path.join(store, "ctx"))
    if proc is None:
        print(f"could not start the transparent base proxy; see {log_path}")
        return 2
    print(f"base arm: transparent proxy on {BASE_PORT}   proxy arm: {PROXY_URL}")
    print(f"{args.runs} runs per arm, {args.turns} scripted turns, model {args.model}\n")

    filler = bulk_payload(args.payload_file, args.payload_bytes)
    results: dict[str, list[dict]] = {"base": [], "proxy": []}
    try:
        for run in range(args.runs):
            for arm, url in (("base", BASE_URL), ("proxy", PROXY_URL)):
                nonce = uuid.uuid4()
                prompts = [
                    f"Run {nonce}. Here is reference material; do not summarise it, "
                    f"just reply with the single word READY.\n\n{filler}"
                ]
                for t in range(args.turns - 1):
                    prompts.append(
                        f"Question {t + 1} of run {nonce}: reply with exactly the "
                        f"word OK and nothing else."
                    )
                row = run_conversation(url, prompts, args.model)
                results[arm].append(row)
                print(f"  run {run + 1} {arm:<6} turns={row['turns']:<3} "
                      f"fresh={row['fresh']:>7,} write={row['write']:>8,} "
                      f"read={row['read']:>10,} fitted={fitted(row):>10,.0f}")
    finally:
        if not args.keep:
            proc.terminate()
            try:
                proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                proc.kill()
            shutil.rmtree(store, ignore_errors=True)

    print()
    means = {}
    for arm in ("base", "proxy"):
        rows = [r for r in results[arm] if r["turns"]]
        if not rows:
            print(f"{arm}: no successful runs")
            return 2
        means[arm] = {k: statistics.mean(r[k] for r in rows) for k in
                      ("fresh", "write", "read", "turns", "usd")}
    print(f"{'arm':8}{'turns':>7}{'fresh':>9}{'write':>10}{'read':>12}{'fitted':>11}{'vs base':>10}")
    base_fit = fitted(means["base"])
    for arm in ("base", "proxy"):
        m = means[arm]
        f = fitted(m)
        delta = "" if arm == "base" else f"{f / base_fit - 1:+.1%}"
        print(f"{arm:8}{m['turns']:>7.2f}{m['fresh']:>9,.0f}{m['write']:>10,.0f}"
              f"{m['read']:>12,.0f}{f:>11,.0f}{delta:>10}")
    spread = {a: (min(fitted(r) for r in results[a] if r["turns"]),
                  max(fitted(r) for r in results[a] if r["turns"])) for a in means}
    print(f"\nwithin-arm spread (fitted): "
          + "  ".join(f"{a} {lo:,.0f}-{hi:,.0f}" for a, (lo, hi) in spread.items()))
    print("A difference smaller than the spread is not a result. Raise --runs.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
