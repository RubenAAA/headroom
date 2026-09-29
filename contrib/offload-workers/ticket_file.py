#!/usr/bin/env python3
"""ticket_file.py: file one YouTrack ticket from the last 10 session turns.

Usage: ticket_file.py SESSION_ID TRANSCRIPT OUTDIR

Mirrors the review offload: a gate hook (ticket-gate.sh) spawns this in
the background, this uses the configured model backend to file through the
b2b-amg ai-youtrack CLI (/create-task procedure), and the outcome
lands in per-session state
files next to the review ones: SESSION.ticket.{context,draft.json,proof.json,
failed,worker.log}. The gate relays the proof on the next user prompt.

Credential rule: the bearer token arrives ONLY as YOUTRACK_TOKEN in the
environment. It is passed to curl from that variable and never echoed,
printed, logged, or written anywhere. Without it the worker fails loud
naming the variable. Dry-run only applies to testing this script by hand --
never run it to completion outside a real diverted session.

Configuration (all in ~/.config/offload-workers/env, mode 600, next to the
token file -- the same file the review chain uses):
  YOUTRACK_URL        YouTrack base URL (required, no default)
  YOUTRACK_PROJECT_ID numeric project id (required, no default --
                        unless per-project ids cover the session cwd)
  YOUTRACK_PROJECT_MAP  comma-separated `path-prefix=id` pairs routing a
                      session cwd to its project (longest prefix wins), e.g.
                      "/home/you/work/alpha=0-21,/home/you/work/beta=0-22".
                      The gate resolves and persists the choice, the worker
                      reads it. A set YOUTRACK_PROJECT_ID still wins over the
                      map (single-project setups keep working).
  AI_YOUTRACK_BIN     ai-youtrack CLI (optional; defaults to the
                      ai-first-workspace checkout path below)
"""

import json
import os
import re
import subprocess
import sys
import tempfile

import model_call as model

ENV_FILE_NOTE = "~/.config/offload-workers/env"


def _req_env(name):
    val = os.environ.get(name, "").strip()
    if not val:
        raise RuntimeError(
            f"ticket filing is not configured: set {name} in {ENV_FILE_NOTE} "
            f"(exported into the hook/worker environment alongside YOUTRACK_TOKEN)")
    return val


def youtrack_url():
    return _req_env("YOUTRACK_URL")


def project_id(outdir=None, session=None):
    """Resolved YouTrack project id: explicit default wins, else cwd routing.

    The gate resolves the session cwd against YOUTRACK_PROJECT_MAP and
    persists the choice in `<session>.ticket.project` next to the divert
    marker; the worker only sees the transcript path, so it reads the
    gate's resolution. A global YOUTRACK_PROJECT_ID still wins when set
    (single-project setups keep working); the map covers the multi-project
    case with no global default.
    """
    val = os.environ.get("YOUTRACK_PROJECT_ID", "").strip()
    if val:
        return val
    if outdir and session:
        try:
            with open(os.path.join(outdir, session + ".ticket.project")) as f:
                routed = f.read().strip()
            if routed:
                return routed
        except OSError:
            pass
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

# ticket-gate.sh supervises the worker for 600s. Keep every model/CLI call
# below that deadline so the worker can record its own timeout and diagnostics.
SUPERVISOR_TIMEOUT = 600
TIMEOUT = min(int(os.environ.get(
    "OFFLOAD_TICKET_TIMEOUT",
    os.environ.get("OFFLOAD_MODEL_TIMEOUT", os.environ.get("SPARK_DRAFT_TIMEOUT", "600")))),
    SUPERVISOR_TIMEOUT - 60)

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
"""


def log(msg):
    print("ticket worker: " + msg, file=sys.stderr, flush=True)


def fail(outdir, session, reason):
    with open(os.path.join(outdir, session + ".ticket.failed"), "w") as f:
        f.write(reason + "\n")
    lines = reason.splitlines() or [""]
    log("failed: " + lines[0])
    for line in lines[1:]:
        log(line)
    return 1


def extract_turns(transcript):
    """Last TURNS user+assistant pairs, speaker-prefixed. jq, not a byte tail.

    Only a tail window is read. A long tool loop can fill a window with
    tool calls and results and no text at all, so the window grows 4x until
    it holds TURNS pairs or covers the whole file. When it stops short of
    the whole file the context carries a truncation warning up front.
    Returns "" when no turn has text.
    """
    size = os.path.getsize(transcript)
    window = WINDOW_BYTES
    while True:
        with open(transcript, "rb") as f:
            if size > window:
                f.seek(-window, os.SEEK_END)
            raw = f.read().decode("utf-8", "replace")
        proc = subprocess.run(
            ["jq", "-R", "-s", TURNS_JQ],
            input=raw, capture_output=True, text=True, timeout=120)
        if proc.returncode != 0:
            raise RuntimeError("turns jq failed: " + proc.stderr.strip()[-300:])
        pairs = json.loads(proc.stdout)
        if len(pairs) >= TURNS or window >= size:
            break
        window *= 4
    if not pairs:
        return ""
    turns = "\n\n---\n\n".join(pairs)
    if size > window:
        turns = ("[warning: transcript truncated to its last %d bytes; "
                 "older turns omitted]\n\n" % window) + turns
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


def build_local_prompt(turns):
    """Ask a local model for ticket fields; the worker runs the CLI itself."""
    return f"""Derive one YouTrack ticket from the session turns below.
Return ONLY one JSON object with these fields:
{{"summary": "one line without emoji", "description": "markdown",
 "type": "default|bug|subtask|epic", "assignee": null,
 "publish": true, "nothing_to_file": false}}

Use type=default unless the turns describe a production defect, name a parent
issue, or ask for a task container. Set assignee only when the turns explicitly
name one. Set publish=false only when the turns explicitly ask to leave a draft.
If there is no filable content, set nothing_to_file=true. Do not perform any
actions, call tools, or claim a ticket was filed. Do not invent facts or
acceptance criteria. The worker will file the ticket after validating this JSON.

Session turns (last {TURNS}, oldest first):
---
{turns}
---
"""


def parse_model_json(output):
    decoder = json.JSONDecoder()
    candidates = [output]
    candidates.extend(re.findall(r"```(?:json)?\s*(.*?)```", output, re.S))
    candidates.extend(re.findall(r"\{.*\}", output, re.S))
    for candidate in candidates:
        try:
            value = json.loads(candidate.strip())
        except (json.JSONDecodeError, AttributeError):
            continue
        if isinstance(value, dict):
            return value
    for offset, char in enumerate(output):
        if char != "{":
            continue
        try:
            value, _ = decoder.raw_decode(output[offset:])
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            return value
    return None


def _clean_cli_error(text):
    token = os.environ.get("YOUTRACK_TOKEN", "")
    text = (text or "").strip()
    if token:
        text = text.replace(token, "[redacted]")
    return text[-500:]


def file_ticket_locally(plan, outdir, bin_path, project):
    """Validate model fields and run create/publish with argv, never a shell.

    Returns (id, error, published). A draft the turns asked to leave unpublished
    is a success with published=False: reporting it as a failure made the next
    "file the ticket" create a second draft.
    """
    summary = plan.get("summary")
    description = plan.get("description")
    kinds = {
        "default": DEFAULT_TYPE,
        "bug": "🚨 Production Bug",
        "subtask": "📋 Subtask",
        "epic": "🏆 Epic",
    }
    kind = plan.get("type", "default")
    if not isinstance(summary, str) or not summary.strip() or "\n" in summary:
        return None, "local model returned an invalid ticket summary", False
    if not isinstance(description, str) or not description.strip():
        return None, "local model returned an invalid ticket description", False
    if not isinstance(kind, str) or kind not in kinds:
        return None, "local model returned an unsupported ticket type", False
    if not isinstance(plan.get("publish"), bool):
        return None, "local model returned a non-boolean publish choice", False
    assignee = plan.get("assignee")
    if assignee is not None and (not isinstance(assignee, str) or not assignee.strip()):
        return None, "local model returned an invalid assignee", False

    description_path = None
    try:
        with tempfile.NamedTemporaryFile(
                mode="w", encoding="utf-8", dir=outdir,
                prefix=".ticket-description-", delete=False) as fh:
            description_path = fh.name
            fh.write(description)
        os.chmod(description_path, 0o600)
        args = [
            "create-draft", f"--project={project}", f"--summary={summary.strip()}",
            f"--description=@{description_path}", f"--type={kinds[kind]}",
            f"--priority={DEFAULT_PRIORITY}", f"--platform={DEFAULT_PLATFORM}",
        ]
        if assignee:
            args.append(f"--assignee={assignee.strip()}")
        created = subprocess.run(
            [bin_path, *args], capture_output=True, text=True,
            timeout=TIMEOUT, env=os.environ.copy())
        if created.returncode != 0:
            return None, "create-draft failed: " + _clean_cli_error(created.stderr), False
        created_json = parse_model_json(created.stdout)
        internal_id = created_json.get("id") if created_json else None
        if not isinstance(internal_id, str) or not internal_id.strip():
            return None, "create-draft returned no internal id", False
        if plan.get("publish", True) is False:
            return internal_id, None, False
        published = subprocess.run(
            [bin_path, "publish-draft", f"--id={internal_id}"],
            capture_output=True, text=True, timeout=TIMEOUT,
            env=os.environ.copy())
        if published.returncode != 0:
            return None, "publish-draft failed: " + _clean_cli_error(published.stderr), False
        # The CLI's JSON names the ticket. Scanning text is the fallback, and
        # only stdout: stderr carries "UTF-8"-shaped noise that matches an id.
        published_json = parse_model_json(published.stdout) or {}
        readable = published_json.get("idReadable")
        if isinstance(readable, str) and re.fullmatch(r"[A-Z][A-Z0-9]*-[0-9]+", readable):
            return readable, None, True
        matches = re.findall(r"\b[A-Z][A-Z0-9]*-[0-9]+\b", published.stdout)
        if not matches:
            return None, "publish-draft returned no readable ticket id", False
        return matches[-1], None, True
    except subprocess.TimeoutExpired:
        return None, f"YouTrack CLI timed out after {TIMEOUT}s", False
    except OSError as error:
        return None, f"could not run YouTrack CLI: {error}", False
    finally:
        if description_path:
            try:
                os.unlink(description_path)
            except OSError:
                pass


def backend_config_error():
    """Return the model routing/consent error before the hook diverts a call."""
    return model.configuration_error()


def main():
    session, transcript, outdir = sys.argv[1], sys.argv[2], sys.argv[3]
    draft_path = os.path.join(outdir, session + ".ticket.draft.json")
    proof_path = os.path.join(outdir, session + ".ticket.proof.json")
    context_path = os.path.join(outdir, session + ".ticket.context")

    config_error = backend_config_error()
    if config_error:
        return fail(outdir, session, config_error)
    if not os.environ.get("YOUTRACK_TOKEN"):
        return fail(outdir, session,
                    "YOUTRACK_TOKEN is not set in the worker environment")
    try:
        cfg_url, cfg_project, cfg_bin = youtrack_url(), project_id(outdir, session), ai_youtrack()
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

    if model.TRANSPORT == "local-direct":
        log("drafting ticket fields with configured local model")
        output, diagnostic = model.call(
            build_local_prompt(turns), TIMEOUT, "OFFLOADED_TICKET_WORKER")
        if output is None:
            return fail(outdir, session, diagnostic or "local model call failed")
        plan = parse_model_json(output)
        if not plan:
            return fail(outdir, session, "local model returned invalid ticket JSON")
        if plan.get("nothing_to_file") is True:
            return fail(outdir, session, "no filable content in session turns")
        log("filing the validated ticket through the YouTrack CLI")
        ticket_id, diagnostic, published = file_ticket_locally(
            plan, outdir, cfg_bin, cfg_project)
        if not ticket_id:
            return fail(outdir, session, diagnostic or "local ticket filing failed")
        if not published:
            # The turns asked for a draft. Record it as the outcome, so the
            # gate reports it and a repeat request does not create another.
            proof = {"session_id": session, "idReadable": "draft " + ticket_id,
                     "url": None, "draft_id": ticket_id,
                     "summary": "draft created and left unpublished, as the turns asked; "
                                "publish it in YouTrack"}
            for path in (draft_path, proof_path):
                with open(path, "w") as f:
                    json.dump(proof, f, indent=2)
            log("draft %s left unpublished; proof at %s" % (ticket_id, proof_path))
            print("proof: " + proof_path)
            return 0
    else:
        prompt = build_prompt(turns, cfg_bin, cfg_project)
        log("filing via configured proxy model")
        output, diagnostic = model.call(
            prompt, TIMEOUT, "OFFLOADED_TICKET_WORKER")
        if output is None:
            return fail(outdir, session, diagnostic or "proxy model call failed")
        ticket_id = ""
        for line in reversed(output.strip().split("\n")):
            line = line.strip()
            if line.startswith("TICKET ") and " " in line:
                ticket_id = line.split(None, 1)[1].strip()
                break
        if (not ticket_id
                or ticket_id in ("NOTHING_TO_FILE", "TOKEN_MISSING", "TOKEN_INVALID")):
            return fail(outdir, session,
                        "no ticket filed; worker said: %s"
                        % (ticket_id or _clean_cli_error(output)))

    # Filing means published: only a readable MC-XXXX id counts. A bare
    # draft id means the publish step never ran -- fail loud, do not
    # report a draft as filed.
    if not re.match(r"^[A-Z]+-[0-9]+$", ticket_id):
        return fail(outdir, session,
                    "draft created but not published; worker said: %s"
                    % _clean_cli_error(ticket_id))
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
    if sys.argv[1:] == ["--check-backend"]:
        error = backend_config_error()
        if error:
            print(error, file=sys.stderr)
            sys.exit(1)
        sys.exit(0)
    sys.exit(main())
