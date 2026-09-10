#!/usr/bin/env python3
"""ticket_file.py: file one YouTrack ticket from the last 10 session turns.

Usage: ticket_file.py SESSION_ID TRANSCRIPT OUTDIR

Mirrors the spark review offload: a gate hook (ticket-gate.sh) spawns this in
the background, this drives one headless `claude -p` call that files through
the b2b-amg ai-youtrack skill's CLI (/create-task procedure), and the outcome
lands in per-session state
files next to the review ones: SESSION.ticket.{context,draft.json,proof.json,
failed,worker.log}. The gate relays the proof on the next user prompt.

Credential rule: the bearer token arrives ONLY as YOUTRACK_TOKEN in the
environment. It is passed to curl from that variable and never echoed,
printed, logged, or written anywhere. Without it the worker fails loud
naming the variable. Dry-run only applies to testing this script by hand --
never run it to completion outside a real diverted session.

Configuration (all in ~/.config/spark-poster/env, mode 600, next to the
token file -- the same file the review chain uses):
  YOUTRACK_URL        YouTrack base URL (required, no default)
  YOUTRACK_PROJECT_ID numeric project id (required, no default)
  AI_YOUTRACK_BIN     ai-youtrack CLI (optional; defaults to the
                      ai-first-workspace checkout path below)
"""

import json
import os
import re
import subprocess
import sys

ENV_FILE_NOTE = "~/.config/spark-poster/env"


def _req_env(name):
    val = os.environ.get(name, "").strip()
    if not val:
        raise RuntimeError(
            f"ticket filing is not configured: set {name} in {ENV_FILE_NOTE} "
            f"(exported into the hook/worker environment alongside YOUTRACK_TOKEN)")
    return val


def youtrack_url():
    return _req_env("YOUTRACK_URL")


def project_id():
    return _req_env("YOUTRACK_PROJECT_ID")


def ai_youtrack():
    val = os.environ.get("AI_YOUTRACK_BIN", "").strip()
    if val:
        return val
    return os.path.join(os.path.expanduser("~"), "meta", "ai-first-workspace",
                        "internal-b2b", "b2b-technology", "platform", "b2b-amg",
                        ".claude", "scripts", "ai-youtrack")


DEFAULT_TYPE = "📜 User Story"  # only 4 live types; Backend & co are archived
DEFAULT_PLATFORM = "Match center"
DEFAULT_PRIORITY = "Normal"

MODEL = os.environ.get("SPARK_DRAFT_MODEL", "claude-muse-spark-1.2")
PROXY = os.environ.get("SPARK_DRAFT_BASE_URL", "http://127.0.0.1:8787")
TIMEOUT = int(os.environ.get("SPARK_DRAFT_TIMEOUT", "600"))

TURNS = 10
WINDOW_BYTES = 120000  # tail window the turns are parsed from, not a byte tail

TURNS_JQ = r"""
split("\n") | map(select(length > 0) | fromjson?)
| map(select(.message.role == "user" or .message.role == "assistant"))
| map({role: .message.role,
      text: (.message.content
             | if type == "string" then .
               elif type == "array"
               then (map(select(.type == "text") | .text // "") | join("\n"))
               else "" end)})
| map(select(.text != ""))
| (reduce .[] as $m ([];
     if $m.role == "user" then . + [{user: $m.text, assistant: []}]
     elif length > 0 then .[-1].assistant += [$m.text]
     else . end))
| .[-10:]
| map("USER:\n\(.user)\n"
      + (if (.assistant | length) > 0
         then "ASSISTANT:\n\(.assistant | join("\n"))"
         else "(no assistant reply yet)" end))
| join("\n\n---\n\n")
"""


def log(msg):
    print("ticket worker: " + msg, file=sys.stderr, flush=True)


def fail(outdir, session, reason):
    with open(os.path.join(outdir, session + ".ticket.failed"), "w") as f:
        f.write(reason + "\n")
    log("failed: " + reason.split("\n")[0])
    return 1


def extract_turns(transcript):
    """Last TURNS user+assistant pairs, speaker-prefixed. jq, not a byte tail.

    Only the tail window is read; when the transcript is bigger than the
    window the context carries a truncation warning up front.
    """
    size = os.path.getsize(transcript)
    with open(transcript, "rb") as f:
        if size > WINDOW_BYTES:
            f.seek(-WINDOW_BYTES, os.SEEK_END)
        raw = f.read().decode("utf-8", "replace")
    proc = subprocess.run(
        ["jq", "-R", "-s", TURNS_JQ],
        input=raw, capture_output=True, text=True, timeout=120)
    if proc.returncode != 0:
        raise RuntimeError("turns jq failed: " + proc.stderr.strip()[-300:])
    turns = json.loads(proc.stdout)
    if size > WINDOW_BYTES:
        turns = ("[warning: transcript truncated to its last %d bytes; "
                 "older turns omitted]\n\n" % WINDOW_BYTES) + turns
    return turns


def build_prompt(turns, bin_path, project):
    return """You file exactly one YouTrack ticket, then stop. Procedure
(b2b-amg /create-task command + ai-youtrack skill):

The divert that started you fired on an explicit "file the ticket"
instruction -- that IS the skill's required confirmation, so you do not
ask again. So you file it PUBLISHED: create the draft (the CLI
requires it as an intermediate step) and publish it in the same run,
unless the turns explicitly say to leave it as a draft.

0. Credential: YOUTRACK_TOKEN (and YOUTRACK_URL) are in your environment
   and the CLI below reads them itself -- values already in the
   environment win over its .env.mcp files. Never print, echo, log, or
   write the token anywhere, including in your final report. If the CLI
   says the token is missing or rejected, stop and print TOKEN_INVALID
   (do not retry, do not print the token).
1. From the session turns below derive a summary (one line, NO emoji --
   the server prepends the Type emoji itself, so an emoji in your summary
   comes out doubled) and a description (markdown: problem, current vs
   expected behavior, affected files, acceptance criteria -- only the
   parts the turns actually support). If the turns hold no filable
   content, print NOTHING_TO_FILE and stop without calling anything.
2. Type: default "TYPE_DEFAULT". Use "TYPE_BUG" when the turns describe a
   production defect, "TYPE_SUBTASK" only when the turns name a parent
   issue, "TYPE_EPIC" only for a container of tasks. NEVER use archived
   types (Backend, Frontend, Dev Bug, Hotfix): the server files them
   under a null summary and the draft fails. When unsure a value exists,
   run "BIN_PLACEHOLDER types" first (--help works offline).
3. File it with the skill's script -- same args a human passes. No curl,
   no reimplementing the API:
   BIN_PLACEHOLDER create-draft --project=PROJECT_ID \\
     --summary="..." --description=... --type="TYPE_DEFAULT" \\
     --priority=PRIORITY_DEFAULT --platform="PLATFORM_DEFAULT"
   (--description takes a literal string, @/path/to/file, or @- on
   stdin. --assignee defaults to the token owner; pass
   --assignee=<login> only when the turns name someone. Do NOT set story
   points, estimation, or tags -- UI-only fields.)
   The CLI prints JSON on stdout (progress on stderr): {"id": "3-XXXXX",
   ...}. A null summary means a rejected field value -- fix the value
   (usually Type) and retry once.
4. Filing means PUBLISHED. The CLI only creates drafts, so do both steps
   in this same run: create-draft first, then immediately
   "BIN_PLACEHOLDER publish-draft --id=<internal-id>" with the draft id
   from step 3 (--id takes the internal id, never --draft-id=). Skip the
   publish only when the turns explicitly say to leave a draft/unpublished.
5. Final line of your output must be exactly: TICKET <readable-id> -- the
   published ticket id (e.g. TICKET MC-1611). Print nothing after it.

Session turns (last TURNS, oldest first):
---
CONTEXT
---
""".replace("BIN_PLACEHOLDER", bin_path).replace(
        "PROJECT_ID", project).replace(
        "TYPE_DEFAULT", DEFAULT_TYPE).replace(
        "TYPE_BUG", "🚨 Production Bug").replace(
        "TYPE_SUBTASK", "📋 Subtask").replace(
        "TYPE_EPIC", "🏆 Epic").replace(
        "PRIORITY_DEFAULT", DEFAULT_PRIORITY).replace(
        "PLATFORM_DEFAULT", DEFAULT_PLATFORM).replace(
        # Last: the turns themselves go in after every config placeholder is
        # resolved, so a turn discussing e.g. PROJECT_ID is never rewritten.
        "TURNS", str(TURNS)).replace("CONTEXT", turns)


def main():
    session, transcript, outdir = sys.argv[1], sys.argv[2], sys.argv[3]
    draft_path = os.path.join(outdir, session + ".ticket.draft.json")
    proof_path = os.path.join(outdir, session + ".ticket.proof.json")
    context_path = os.path.join(outdir, session + ".ticket.context")

    if not os.environ.get("YOUTRACK_TOKEN"):
        return fail(outdir, session,
                    "YOUTRACK_TOKEN is not set in the worker environment")
    try:
        cfg_url, cfg_project, cfg_bin = youtrack_url(), project_id(), ai_youtrack()
    except RuntimeError as e:
        return fail(outdir, session, str(e))
    log("extracting last %d turns" % TURNS)
    try:
        turns = extract_turns(transcript)
    except Exception as e:
        return fail(outdir, session, "turns extraction failed: %s" % e)
    if not turns.strip():
        return fail(outdir, session, "no user/assistant turns in transcript")
    with open(context_path, "w") as f:
        f.write(turns)
    log("context at %s (%d bytes)" % (context_path, len(turns.encode())))

    prompt = build_prompt(turns, cfg_bin, cfg_project)
    env = dict(os.environ, TICKET_FILE_WORKER="1", ANTHROPIC_BASE_URL=PROXY)
    log("filing via headless model %s" % MODEL)
    try:
        proc = subprocess.run(
            ["claude", "-p", "--model", MODEL, prompt],
            capture_output=True, text=True, timeout=TIMEOUT, env=env)
    except subprocess.TimeoutExpired:
        return fail(outdir, session,
                    "headless call timed out after %ds" % TIMEOUT)
    if proc.returncode != 0:
        tail = proc.stderr.strip().split("\n")[-3:]
        return fail(outdir, session,
                    "headless call failed (rc=%d): %s"
                    % (proc.returncode, " | ".join(tail)))

    ticket_id = ""
    for line in reversed(proc.stdout.strip().split("\n")):
        line = line.strip()
        if line.startswith("TICKET ") and " " in line:
            ticket_id = line.split(None, 1)[1].strip()
            break
    if not ticket_id or ticket_id in ("NOTHING_TO_FILE", "TOKEN_MISSING", "TOKEN_INVALID"):
        return fail(outdir, session,
                    "no ticket filed; worker said: %s"
                    % (ticket_id or proc.stdout.strip()[-300:]))

    # Filing means published: only a readable MC-XXXX id counts. A bare
    # draft id means the publish step never ran -- fail loud, do not
    # report a draft as filed.
    if not re.match(r"^[A-Z]+-[0-9]+$", ticket_id):
        return fail(outdir, session,
                    "draft created but not published; worker said: %s"
                    % (ticket_id or proc.stdout.strip()[-300:]))
    url = cfg_url + "/issue/" + ticket_id
    draft = {"session_id": session, "idReadable": ticket_id, "url": url}
    with open(draft_path, "w") as f:
        json.dump(draft, f, indent=2)
    proof = dict(draft, summary="ticket filed and published from session turns")
    with open(proof_path, "w") as f:
        json.dump(proof, f, indent=2)
    log("filed %s; proof at %s" % (ticket_id, proof_path))
    print("proof: " + proof_path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
