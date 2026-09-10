"""Thin GitLab transport shared by the reader and the poster.

Why a module and not a `glab` call: bracketed form-field keys do not survive
`glab api -f` encoding, so anything carrying a `position` silently posts as an
unanchored general comment -- 201, note visible, `position: null`. Replies to
an existing discussion need no position and would survive `glab`, but keeping
one transport for both halves means the reader and the poster cannot disagree
about auth, proxy or base URL.

Egress goes through `rtk proxy curl` because the GitLab host is only reachable
that way from this box.

Which host and project: environment, never this file. A hardcoded host or
project path ships the author's employer in every checkout, and neither
works for anyone else. See README's Environment table.
"""

import json
import os
import subprocess
import urllib.parse

TOKEN_FILE = os.path.expanduser("~/.config/spark-poster/token")


def _config():
    """(base, project, encoded project), or a loud failure naming the vars.

    Validated on every call rather than at import so `--help`-style entry
    points and unrelated imports never die on configuration.
    """
    base = os.environ.get("SPARK_GITLAB_BASE_URL", "").strip().rstrip("/")
    project = os.environ.get("SPARK_GITLAB_PROJECT", "").strip().strip("/")
    if not base or not project:
        raise RuntimeError(
            "spark-poster is not pointed at a GitLab: set SPARK_GITLAB_BASE_URL "
            "(e.g. https://gitlab.example.com/api/v4) and SPARK_GITLAB_PROJECT "
            "(e.g. group/sub/project) in ~/.config/spark-poster/env"
        )
    return base, project, urllib.parse.quote(project, safe="")


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
    base, _, _ = _config()
    url = f"{base}{path}"
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


def project():
    """(web base, project path) for display URLs. The API base ends in
    /api/v4; the web UI is everything before it."""
    base, project, _ = _config()
    return base.removesuffix("/api/v4"), project


def proj_enc():
    """URL-encoded project path for `/projects/...` routes."""
    return _config()[2]


def discussions(iid):
    """Every discussion on the MR, following pagination."""
    _, _, proj_enc = _config()
    out, page = [], 1
    while True:
        status, batch = call(
            "GET",
            f"/projects/{proj_enc}/merge_requests/{iid}/discussions"
            f"?per_page=100&page={page}",
        )
        if status != 200 or not isinstance(batch, list) or not batch:
            break
        out.extend(batch)
        if len(batch) < 100:
            break
        page += 1
    return out


def discussion(iid, discussion_id):
    """One discussion, fresh. The resolve confirmation reads this instead of
    trusting the batch verdict against an earlier snapshot."""
    _, _, proj_enc = _config()
    status, disc = call(
        "GET",
        f"/projects/{proj_enc}/merge_requests/{iid}/discussions/{discussion_id}",
    )
    return disc if status == 200 and isinstance(disc, dict) else None


def merge_request(iid):
    _, _, proj_enc = _config()
    status, mr = call("GET", f"/projects/{proj_enc}/merge_requests/{iid}")
    return mr if status == 200 else None


def resolve(iid, discussion_id, resolved=True):
    """Close a thread, or reopen it.

    Separate from `reply` on purpose: a verdict is posted whether or not the
    thread closes, and closing without saying why is the thing this whole
    exercise exists to stop.
    """
    _, _, proj_enc = _config()
    return call(
        "PUT",
        f"/projects/{proj_enc}/merge_requests/{iid}/discussions/{discussion_id}"
        f"?resolved={'true' if resolved else 'false'}",
    )


def reply(iid, discussion_id, body):
    """Append a note to an existing thread. No position: it inherits the
    thread's anchor, which is why replies cannot suffer the `position: null`
    failure that afflicts new inline threads."""
    _, _, proj_enc = _config()
    return call(
        "POST",
        f"/projects/{proj_enc}/merge_requests/{iid}/discussions/{discussion_id}/notes",
        {"body": body},
    )


def diff_refs(iid):
    """The SHAs a new inline thread must anchor to, read at post time.

    The draft is evidence against one head and the poster refuses to send it
    against another, but the anchor SHAs still come from the live MR, so a
    position can never cite a stale base."""
    mr = merge_request(iid) or {}
    refs = mr.get("diff_refs") or {}
    if not all(refs.get(k) for k in ("base_sha", "head_sha", "start_sha")):
        return None
    return {"base_sha": refs["base_sha"], "head_sha": refs["head_sha"],
            "start_sha": refs["start_sha"]}


def create_discussion(iid, body, position=None):
    """Open a new thread. Without a position it is a plain MR-level
    discussion; with one it is an inline thread on the diff.

    The position is the caller's to build (SHAs from diff_refs at post
    time); this function sends exactly what it is given. A position GitLab
    cannot anchor fails here with its status, loudly -- falling back to a
    plain discussion would repeat the silent-mispost class this module
    exists to prevent."""
    _, _, proj_enc = _config()
    payload = {"body": body}
    if position is not None:
        payload["position"] = position
    return call(
        "POST",
        f"/projects/{proj_enc}/merge_requests/{iid}/discussions",
        payload,
    )
