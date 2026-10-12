#!/usr/bin/env python3
"""ticket_file.py: file one YouTrack ticket from the last 10 session turns.

Usage: ticket_file.py SESSION_ID TRANSCRIPT OUTDIR

Mirrors the review offload: a gate hook (ticket-gate.sh) spawns this in
the background, the configured model drafts the ticket fields as JSON with
no tools, this files them through the b2b-amg ai-youtrack CLI, and the outcome
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
import time

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


DEFAULT_PLATFORM = "Match center"  # sent only to projects whose issues carry it
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
               then (map(if .type == "text" then .text // ""
                         elif .type == "tool_use" and (.name == "Write" or .name == "Edit")
                         then "[\(.name) \(.input.file_path // "?")]"
                         else empty end) | join("\n"))
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


WRITTEN_FILES = 3          # newest files the turns wrote, inlined for the model
WRITTEN_FILE_BYTES = 40000


def written_files(turns):
    """Current text of the newest files the turns wrote ([Write /p], [Edit /p]).

    A session often drafts the ticket text in a file. Inlining it saves the
    model a tool call; the agent that had to fetch it once read an older
    ticket's draft and then searched the raw transcript for minutes.
    """
    paths = []
    for path in reversed(re.findall(r"^\[(?:Write|Edit) ([^\]]+)\]$", turns, re.M)):
        if path not in paths:
            paths.append(path)
    out = []
    for path in paths[:WRITTEN_FILES]:
        try:
            if os.path.getsize(path) > WRITTEN_FILE_BYTES:
                continue
            with open(path, encoding="utf-8", errors="replace") as f:
                out.append("Current content of %s:\n```\n%s\n```" % (path, f.read()))
        except OSError:
            continue
    return "\n\n".join(out)


def build_prompt(turns):
    """Ask the model for ticket fields only; the worker files them itself.

    An agent that ran the YouTrack CLI took 30 turns over 9 minutes, broke
    the summary's shell quoting, and "tested" an update by retitling the
    published ticket to "X". A JSON draft is one call and cannot do that.
    """
    files = written_files(turns)
    files = ("\n\nFiles the session wrote, newest first. A file can belong to "
             "an earlier, different ticket; use one only if it matches the "
             "latest request.\n\n" + files) if files else ""
    return f"""Draft one YouTrack ticket from the session turns below.
The user's latest request to file a ticket decides what the ticket is about;
earlier turns are background. Return ONLY one JSON object:
{{"summary": "one line, no emoji", "description": "markdown",
 "type": "default|bug|subtask|epic", "assignee": null,
 "publish": true, "nothing_to_file": false}}

summary: one line in the language the session uses for this work.
description: problem, current vs expected behavior, affected files,
acceptance criteria -- only what the turns support. Do not invent facts.
type: default unless the turns describe a production defect (bug), name a
parent issue (subtask), or ask for a container of tasks (epic).
assignee: a login only when the turns name one.
publish: false only when the turns explicitly ask to leave a draft.
nothing_to_file: true when the turns hold nothing to file.
Do not call tools or claim anything was filed: the worker files the JSON.

Session turns (last {TURNS}, oldest first):
---
{turns}
---{files}
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


def _redact(text):
    token = os.environ.get("YOUTRACK_TOKEN", "")
    text = (text or "").strip()
    if token:
        text = text.replace(token, "[redacted]")
    return text


def _clean_cli_error(text):
    return _redact(text)[-500:]


def filed_since(bin_path, project, since_ms):
    """Published issues the token owner created in PROJECT since SINCE_MS.

    YouTrack, not the agent's reply, says whether a run filed anything. A
    reply that lost its TICKET line once recorded a filed epic as a failure,
    and the retry that followed could have filed it twice. Returns None
    when YouTrack cannot be asked.
    """
    def cli(*args):
        proc = subprocess.run([bin_path, *args], capture_output=True, text=True,
                              timeout=120, env=os.environ.copy())
        return json.loads(proc.stdout) if proc.returncode == 0 else None
    try:
        projects = cli("list-projects")
        short = next((p.get("shortName") for p in projects or []
                      if isinstance(p, dict) and p.get("id") == project), None)
        if not short:
            return None
        # A day early: the query's dates are in the account's timezone.
        day = time.strftime("%Y-%m-%d", time.localtime(since_ms / 1000 - 86400))
        issues = cli("search", "--project=" + short,
                     "--query=reporter: me created: %s .. Today" % day)
    except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError):
        return None
    if not isinstance(issues, list):
        return None
    return [i for i in issues if isinstance(i, dict) and i.get("idReadable")
            and isinstance(i.get("created"), int) and i["created"] >= since_ms]


# Type names per kind, as projects spell them once the emoji is stripped.
KIND_TYPES = {
    "default": ("User Story", "Task"),
    "bug": ("Production Bug", "Bug"),
    "subtask": ("Subtask",),
    "epic": ("Epic",),
}


def project_fields(bin_path, project):
    """Type names and Platform values on the project's recent issues.

    Projects differ: MC spells types with an emoji ("📜 User Story") and has a
    Platform field; ANL spells them plain and has none. `get-fields` comes
    back empty for this token, so the live issues are the source. Returns
    (types, platforms), or None when YouTrack cannot be asked.
    """
    def cli(*args):
        proc = subprocess.run([bin_path, *args], capture_output=True, text=True,
                              timeout=120, env=os.environ.copy())
        return json.loads(proc.stdout) if proc.returncode == 0 else None
    try:
        short = next((p.get("shortName") for p in cli("list-projects") or []
                      if isinstance(p, dict) and p.get("id") == project), None)
        if not short:
            return None
        # Recent issues only: archived types live on old ones.
        day = time.strftime("%Y-%m-%d", time.localtime(time.time() - 60 * 86400))
        issues = cli("search", "--project=" + short,
                     "--query=created: %s .. Today" % day, "--page-size=100")
    except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError):
        return None
    if not isinstance(issues, list):
        return None
    types, platforms = set(), set()
    for issue in issues:
        for field in issue.get("customFields") or [] if isinstance(issue, dict) else []:
            value = field.get("value") if isinstance(field, dict) else None
            name = value.get("name") if isinstance(value, dict) else None
            if name and field.get("name") == "Type":
                types.add(name)
            elif name and field.get("name") == "Platform":
                platforms.add(name)
    return types, platforms


def resolve_type(kind, types):
    """The project's own spelling of KIND, or None to take its default."""
    for want in KIND_TYPES[kind]:
        for name in sorted(types):
            if re.sub(r"^[^\w]+", "", name).strip().lower() == want.lower():
                return name
    return None


def file_ticket_locally(plan, outdir, bin_path, project):
    """Validate model fields and run create/publish with argv, never a shell.

    Returns (id, error, published). A draft the turns asked to leave unpublished
    is a success with published=False: reporting it as a failure made the next
    "file the ticket" create a second draft.
    """
    summary = plan.get("summary")
    description = plan.get("description")
    kind = plan.get("type", "default")
    if not isinstance(summary, str) or not summary.strip() or "\n" in summary:
        return None, "model returned an invalid ticket summary", False
    if not isinstance(description, str) or not description.strip():
        return None, "model returned an invalid ticket description", False
    if not isinstance(kind, str) or kind not in KIND_TYPES:
        return None, "model returned an unsupported ticket type", False
    if not isinstance(plan.get("publish"), bool):
        return None, "model returned a non-boolean publish choice", False
    assignee = plan.get("assignee")
    if assignee is not None and (not isinstance(assignee, str) or not assignee.strip()):
        return None, "model returned an invalid assignee", False
    fields = project_fields(bin_path, project)
    if fields is None:
        return None, "could not read the project's issue types from YouTrack", False
    types, platforms = fields

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
            f"--description=@{description_path}", f"--priority={DEFAULT_PRIORITY}",
        ]
        # No match leaves the field to the project's default rather than
        # sending a value its bundle rejects.
        type_name = resolve_type(kind, types)
        if type_name:
            args.append(f"--type={type_name}")
        if DEFAULT_PLATFORM in platforms:
            args.append(f"--platform={DEFAULT_PLATFORM}")
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

    # One ticket per session: anything the token owner filed in the project
    # since the first attempt is this session's ticket. Every run checks
    # first, so a run that filed and then failed (lost id, timeout) is not
    # filed again. ticket-gate.sh stamps the first attempt when the session
    # asks; the fallback here covers a run started by hand.
    attempt_path = os.path.join(outdir, session + ".ticket.first-attempt")
    if not os.path.exists(attempt_path):
        open(attempt_path, "a").close()
    since_ms = int(os.path.getmtime(attempt_path) * 1000) - 120000  # clock skew

    def record(ticket_id, note):
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
        proof = dict(draft, summary=note)
        with open(proof_path, "w") as f:
            json.dump(proof, f, indent=2)
        log("filed %s; proof at %s" % (ticket_id, proof_path))
        print("proof: " + proof_path)
        return 0

    def adopt(found, why):
        if len(found) > 1:
            return fail(outdir, session,
                        "%s; several tickets filed since the first attempt, check for "
                        "duplicates: %s" % (why, ", ".join(i["idReadable"] for i in found)))
        return record(found[0]["idReadable"],
                      "%s; found in YouTrack: %s" % (why, found[0].get("summary") or ""))

    log("checking YouTrack for a ticket an earlier attempt filed")
    found = filed_since(cfg_bin, cfg_project, since_ms)
    if found:
        return adopt(found, "filed by an earlier attempt, not filed again")

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

    log("drafting ticket fields with the configured model")
    output, diagnostic = model.call(
        build_prompt(turns), TIMEOUT, "OFFLOADED_TICKET_WORKER", tools="")
    if output is None:
        return fail(outdir, session, diagnostic or "model call failed")
    plan = parse_model_json(output)
    if not plan:
        return fail(outdir, session, "model returned invalid ticket JSON")
    if plan.get("nothing_to_file") is True:
        return fail(outdir, session, "no filable content in session turns")
    log("filing the validated ticket through the YouTrack CLI")
    ticket_id, diagnostic, published = file_ticket_locally(
        plan, outdir, cfg_bin, cfg_project)
    if not ticket_id:
        return fail(outdir, session, diagnostic or "ticket filing failed")
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

    return record(ticket_id, "ticket filed and published from session turns")


if __name__ == "__main__":
    if sys.argv[1:] == ["--check-backend"]:
        error = backend_config_error()
        if error:
            print(error, file=sys.stderr)
            sys.exit(1)
        sys.exit(0)
    sys.exit(main())
