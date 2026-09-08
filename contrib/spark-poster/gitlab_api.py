"""Thin GitLab transport shared by the reader and the poster.

Why a module and not a `glab` call: bracketed form-field keys do not survive
`glab api -f` encoding, so anything carrying a `position` silently posts as an
unanchored general comment -- 201, note visible, `position: null`. Replies to
an existing discussion need no position and would survive `glab`, but keeping
one transport for both halves means the reader and the poster cannot disagree
about auth, proxy or base URL.

Egress goes through `rtk proxy curl` because the GitLab host is only reachable
that way from this box.
"""

import json
import os
import subprocess
import urllib.parse

BASE = "https://rantsports.gitlab.yandexcloud.net/api/v4"
PROJECT = "ai-first-workspace/internal-b2b/b2b-technology/platform/b2b-amg"
PROJ_ENC = urllib.parse.quote(PROJECT, safe="")

TOKEN_FILE = os.path.expanduser("~/.config/spark-poster/token")


class TokenMissing(RuntimeError):
    pass


def token():
    """The worker's credential, from its own file first.

    Falls back to the ambient environment only so this keeps working before
    the credential move; once the token leaves `.bashrc` the environment
    branch is dead and Opus's shell has nothing to offer.
    """
    try:
        with open(TOKEN_FILE) as fh:
            tok = fh.read().strip()
            if tok:
                return tok
    except OSError:
        pass
    tok = os.environ.get("GITLAB_TOKEN", "").strip()
    if not tok:
        raise TokenMissing(
            f"no credential: {TOKEN_FILE} is absent or empty and GITLAB_TOKEN is unset"
        )
    return tok


def call(method, path, payload=None, timeout=60):
    """One API call. Returns (status, parsed_body).

    The body is written to a temp file and handed to curl as `--data-binary @`
    rather than interpolated into the command line, so a note body containing
    quotes or newlines cannot reshape the command.
    """
    url = f"{BASE}{path}"
    cmd = [
        "rtk", "proxy", "curl", "-s",
        "-w", "\n%{http_code}",
        "-X", method, url,
        "-H", f"PRIVATE-TOKEN: {token()}",
    ]
    tmp = None
    if payload is not None:
        tmp = f"/tmp/.spark-payload-{os.getpid()}.json"
        with open(tmp, "w") as fh:
            json.dump(payload, fh)
        os.chmod(tmp, 0o600)
        cmd += ["-H", "Content-Type: application/json", "--data-binary", f"@{tmp}"]
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout).stdout
    finally:
        if tmp:
            try:
                os.unlink(tmp)
            except OSError:
                pass
    body, _, status = out.rpartition("\n")
    try:
        parsed = json.loads(body) if body.strip() else None
    except json.JSONDecodeError:
        parsed = body
    return int(status or 0), parsed


def me():
    """Whose threads are 'mine', asked of the server rather than assumed.

    This was hardcoded to a guessed username once, and the guess was another
    reviewer on the same MR -- so the triage read their threads, and a reply
    went onto one of them. The credential already knows who it belongs to;
    there is no reason for anything here to hold an opinion about it.
    """
    status, user = call("GET", "/user")
    if status != 200 or not isinstance(user, dict) or not user.get("username"):
        raise RuntimeError(f"could not identify the token's owner (status {status})")
    return user["username"]


def discussions(iid):
    """Every discussion on the MR, following pagination."""
    out, page = [], 1
    while True:
        status, batch = call(
            "GET",
            f"/projects/{PROJ_ENC}/merge_requests/{iid}/discussions"
            f"?per_page=100&page={page}",
        )
        if status != 200 or not isinstance(batch, list) or not batch:
            break
        out.extend(batch)
        if len(batch) < 100:
            break
        page += 1
    return out


def merge_request(iid):
    status, mr = call("GET", f"/projects/{PROJ_ENC}/merge_requests/{iid}")
    return mr if status == 200 else None


def reply(iid, discussion_id, body):
    """Append a note to an existing thread. No position: it inherits the
    thread's anchor, which is why replies cannot suffer the `position: null`
    failure that afflicts new inline threads."""
    return call(
        "POST",
        f"/projects/{PROJ_ENC}/merge_requests/{iid}/discussions/{discussion_id}/notes",
        {"body": body},
    )
