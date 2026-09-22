#!/usr/bin/env python3
"""Behaviour analysis for one bench pane.

Reads the Claude Code session transcript and the proxy log, and answers the
questions a human would otherwise ask by watching: did it produce anything,
did it use tools sanely, did it finish, or is it going round in circles.

Task completion is graded separately, from files on disk (tasks.sh). This
script judges behaviour, not correctness.

One rule is baked in deliberately. Liveness is measured from transcript
records and tool calls, never from turn counters. On 2026-09-22 a Grok pane
held one turn open for four minutes of reasoning; the turn counter sat flat
and looked exactly like a hang. Turn counters only move between turns, so a
high-effort model appears dead while it is working.
"""

import argparse
import collections
import hashlib
import json
import os
from datetime import datetime, timezone

CIRCLING_REPEATS = 3


def parse_ts(value):
    if not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def blocks_of(message):
    content = message.get("content")
    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    return content if isinstance(content, list) else []


def read_transcript(path):
    records = []
    if not os.path.exists(path):
        return records
    with open(path, errors="replace") as fh:
        for line in fh:
            try:
                records.append(json.loads(line))
            except ValueError:
                continue
    return records


def starts_new_turn(rec):
    """True for a real user prompt, false for a tool result coming back.

    Claude Code delivers tool results as `user` records, so a naive "user
    record ends the turn" rule would cut every turn at its first tool call.
    A real prompt has string content or non-tool_result blocks.
    """
    if rec.get("type") != "user":
        return False
    message = rec.get("message")
    if not isinstance(message, dict):
        return False
    content = message.get("content")
    if isinstance(content, str):
        return True
    if not isinstance(content, list):
        return False
    kinds = {b.get("type") for b in content if isinstance(b, dict)}
    return kinds != {"tool_result"}


def analyse_turns(records, start=None, end=None):
    """Per-window behaviour, grouped into turns rather than records.

    Claude Code writes one assistant record per block type: a thinking record,
    then a text record, then a tool_use record, all inside one turn. Counting
    a thinking-only *record* as a blank turn reports every healthy session as
    broken — measured at 31 false positives in a 239-record session. A blank
    turn is a whole turn that produced thinking and neither text nor a tool
    call, which is the 2026-09-22 Zen symptom.
    """
    tools = []
    turns = []
    current = None
    last_activity = None

    def close(turn):
        if turn is not None and (turn["text"] or turn["thought"] or turn["tools"]):
            turns.append(turn)

    for rec in records:
        ts = parse_ts(rec.get("timestamp"))
        if ts and start and ts < start:
            continue
        if ts and end and ts > end:
            continue
        if ts:
            last_activity = ts if last_activity is None else max(last_activity, ts)

        if starts_new_turn(rec):
            close(current)
            current = {"text": "", "thought": False, "tools": 0}
            continue
        if rec.get("type") != "assistant":
            continue
        message = rec.get("message")
        if not isinstance(message, dict):
            continue
        if current is None:
            current = {"text": "", "thought": False, "tools": 0}
        for block in blocks_of(message):
            if not isinstance(block, dict):
                continue
            kind = block.get("type")
            if kind == "text":
                current["text"] += block.get("text") or ""
            elif kind == "thinking":
                current["thought"] = True
            elif kind == "tool_use":
                current["tools"] += 1
                args = json.dumps(block.get("input"), sort_keys=True, default=str)
                tools.append(
                    {
                        "name": block.get("name"),
                        "args_hash": hashlib.sha1(args.encode()).hexdigest()[:12],
                        "args_head": args[:160],
                        "ts": rec.get("timestamp"),
                    }
                )
    close(current)

    spoke = [t["text"].strip() for t in turns if t["text"].strip()]
    served = collections.Counter()
    for rec in records:
        ts = parse_ts(rec.get("timestamp"))
        if ts and start and ts < start:
            continue
        if ts and end and ts > end:
            continue
        message = rec.get("message")
        if rec.get("type") == "assistant" and isinstance(message, dict):
            if message.get("model"):
                served[message["model"]] += 1
    blank = [t for t in turns if t["thought"] and not t["text"].strip() and not t["tools"]]

    return {
        "assistant_turns": len(turns),
        "tool_calls": len(tools),
        "tool_mix": dict(collections.Counter(t["name"] for t in tools)),
        "blank_turns": len(blank),
        "visible_text_turns": len(spoke),
        "last_text": spoke[-1][:300] if spoke else "",
        "last_activity": last_activity.isoformat() if last_activity else None,
        # Which model actually answered, read back from the transcript rather
        # than assumed from what the pane was asked to use. A pane whose
        # `/model` switch silently failed will otherwise report clean passes
        # for a model that never ran — observed 2026-09-22, when a Grok pane
        # scored two completions that claude-opus-5 had served.
        "served_by": dict(served),
        "_tools": tools,
    }


def find_circling(tools):
    """A model re-running one call with identical arguments is not working.

    Reported with the arguments, because the proxy log carries tool names but
    not their inputs — which is the gap that made a real investigation
    indistinguishable from a loop when this was diagnosed by log alone.
    """
    repeats = collections.Counter((t["name"], t["args_hash"]) for t in tools)
    offenders = []
    for (name, args_hash), count in repeats.items():
        if count >= CIRCLING_REPEATS:
            head = next(t["args_head"] for t in tools if t["args_hash"] == args_hash)
            offenders.append({"tool": name, "count": count, "args_head": head})
    return sorted(offenders, key=lambda o: -o["count"])


def proxy_faults(log_path, upstream_model, start, end):
    """Proxy-side failures in the window, so a bad turn can be blamed correctly.

    Attribution is by request, not by window alone. Fault records carry a
    `request_id` and no model, so the model is recovered from the other
    records of the same request — `model_routing_decision` and the savings
    ledger both name it. Counting every fault in the window against every
    pane put four Zen 403s on the sonnet control row, which is the one row
    the report exists to keep clean.

    Two panes of the same model, or another session on it, still merge; keep
    bench runs clean or read this as an upper bound.
    """
    faults = collections.Counter()
    if not os.path.exists(log_path):
        return dict(faults)

    markers = {
        "continuation_non_2xx": "upstream returned error during continuation",
        "fold_failed": "failed to parse continuation response",
        "served_fallback": "serving store-fetched content after continuation failure",
        "tool_call_dropped": "dropped a proxy tool call the client expected",
        "empty_turn_notice": "turn promised a tool call the client will not receive",
    }
    start_s = start.isoformat().replace("+00:00", "Z") if start else ""
    end_s = end.isoformat().replace("+00:00", "Z") if end else "9999"

    seen_model = False
    # request_id -> was this model named anywhere in that request's records.
    request_is_ours = {}
    hits = []
    with open(log_path, errors="replace") as fh:
        for line in fh:
            if '"timestamp"' not in line:
                continue
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            ts = rec.get("timestamp", "")
            if ts < start_s or ts > end_s:
                continue
            fields = rec.get("fields", {})
            blob = json.dumps(fields)
            mine = upstream_model in blob
            if mine:
                seen_model = True
            rid = fields.get("request_id")
            if rid:
                request_is_ours[rid] = request_is_ours.get(rid, False) or mine
            message = fields.get("message", "")
            for key, needle in markers.items():
                if needle in message:
                    hits.append((key, rid))

    for key, rid in hits:
        # A fault whose request never named a model is nobody's to claim. It
        # is dropped rather than shared out, so a clean pane reads clean.
        if rid and request_is_ours.get(rid):
            faults[key] += 1
    faults["saw_model_traffic"] = 1 if seen_model else 0
    return dict(faults)


def verdict(stats, circling, idle_seconds, budget_exceeded, artifact_pass):
    if circling:
        return "circling"
    if stats["blank_turns"] and not stats["visible_text_turns"]:
        return "blank"
    if budget_exceeded and idle_seconds is not None and idle_seconds > 90:
        return "hung"
    if budget_exceeded:
        return "over_budget"
    if artifact_pass is True:
        return "completed"
    if artifact_pass is False:
        return "wrong"
    return "running"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--transcript", required=True)
    ap.add_argument("--upstream-model", required=True)
    ap.add_argument("--tasklog", help="tsv written by the driver: task, sent, done")
    ap.add_argument("--proxy-log", default=os.path.expanduser("~/headroom-proxy.log"))
    ap.add_argument(
        "--compact",
        action="store_true",
        help="one line of JSON, so callers can carry it in a tab-separated field",
    )
    args = ap.parse_args()

    records = read_transcript(args.transcript)
    now = datetime.now(timezone.utc)

    windows = []
    if args.tasklog and os.path.exists(args.tasklog):
        for line in open(args.tasklog):
            parts = line.rstrip("\n").split("\t")
            if len(parts) >= 2:
                task, sent = parts[0], parts[1]
                done = parts[2] if len(parts) > 2 and parts[2] else None
                over = parts[3] == "over" if len(parts) > 3 else False
                windows.append((task, parse_ts(sent), parse_ts(done), over))

    out = {"transcript": args.transcript, "exists": bool(records), "tasks": []}

    for task, start, end, over in windows:
        stats = analyse_turns(records, start, end)
        circling = find_circling(stats.pop("_tools"))
        last = parse_ts(stats["last_activity"])
        idle = (now - last).total_seconds() if last else None
        stats.update(
            {
                "task": task,
                "circling": circling,
                "idle_seconds": round(idle) if idle is not None else None,
                "proxy_faults": proxy_faults(
                    args.proxy_log, args.upstream_model, start, end or now
                ),
                "verdict": verdict(stats, circling, idle, over, None),
            }
        )
        out["tasks"].append(stats)

    if not windows:
        stats = analyse_turns(records)
        stats["circling"] = find_circling(stats.pop("_tools"))
        out["tasks"].append(stats)

    print(json.dumps(out) if args.compact else json.dumps(out, indent=2))


if __name__ == "__main__":
    main()
