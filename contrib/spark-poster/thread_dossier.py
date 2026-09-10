"""One dossier per review thread: what I asked, and what the code says now.

Pairs each thread's opening note with the current state of the lines it was
anchored to, plus whether that file moved in the commits that postdate the
review. That is the whole evidence base for "was this addressed" -- the
alternative, judging from the thread alone, cannot see a fix that landed as a
commit rather than a reply.
"""

import os
import subprocess
import sys

import gitlab_api as gl

ME = gl.me()
# The repo under review. Deliberately no default: the author's tree used to
# sit here as a literal path, which shipped a personal directory layout in
# every checkout and silently broke everyone else. Set SPARK_REPO (the review
# hook sources ~/.config/spark-poster/env into the worker, so interactive
# sessions keep working with no extra setup).
REPO = os.environ.get("SPARK_REPO", "").strip()
HEAD = os.environ.get("SPARK_HEAD", "FETCH_HEAD")
CONTEXT = 25


def git(*args):
    r = subprocess.run(["git", "-C", REPO, *args], capture_output=True, text=True)
    return r.stdout


def select_threads(discussions, me, mode):
    """Which threads get replies, by command. Pure -- takes what the API
    returned, fetches nothing, so it stays testable without a token.

    gitlab-review: follow-ups on threads I opened. My objections, their
    answers; the verdict is whether the objection still stands.

    fix-mr-comments: reviewers' threads on my MR that are still open. On my
    own MR there is nothing to draft on a thread I opened myself -- those
    are self-notes -- and a resolved thread needs no reply. Scoping this to
    "threads I opened" drafted nothing on exactly the MRs the command is
    for (MR !554: 44 threads, 20 open, zero opened by me).
    """
    ds = [d for d in discussions
          if not d.get("individual_note") and d.get("notes")]
    if mode == "fix-mr-comments":
        return [d for d in ds
                if not d.get("resolved")
                and d["notes"][0].get("author", {}).get("username") != me]
    return [d for d in ds
            if d["notes"][0].get("author", {}).get("username") == me]


def my_threads(iid, mode="gitlab-review"):
    """The in-scope threads, plus the timestamp of the earliest opening note.

    Shared with the drafter so both halves agree on whose threads are in scope;
    a drafter that disagreed with the dossier about that is how a reply once
    landed on another reviewer's thread.
    """
    if not REPO:
        raise SystemExit(
            "SPARK_REPO is unset: point it at the repo under review "
            "(the hook loads ~/.config/spark-poster/env for workers)")
    ds = [d for d in gl.discussions(iid) if not d.get("individual_note")]
    mine = select_threads(ds, ME, mode)
    if not mine:
        return [], None
    return mine, min(d["notes"][0]["created_at"] for d in mine)


def resolve_head(iid):
    """The commit the evidence must describe: the MR's current head.

    The dossier used to read whatever HEAD happened to point at (default:
    FETCH_HEAD, i.e. the last fetch of anything). A fix pushed 20 minutes
    before the run was therefore invisible: verdicts said "nothing
    changed", cited paths didn't exist at the head, and two threads got
    resolved on that basis. Now the server names the head SHA, the source
    branch is fetched, and anything less than the object on disk is a loud
    refusal instead of a confident wrong draft.
    """
    mr = gl.merge_request(iid) or {}
    sha = mr.get("sha") or ""
    branch = mr.get("source_branch") or ""
    if not sha or not branch:
        raise SystemExit(
            f"MR !{iid}: server gave no head sha/branch; not drafting blind")
    git("fetch", "origin", branch)
    probe = subprocess.run(["git", "-C", REPO, "cat-file", "-e", sha],
                           capture_output=True)
    if probe.returncode != 0:
        raise SystemExit(
            f"MR !{iid}: head {sha[:12]} not fetchable from origin/{branch}; "
            f"not drafting")
    return sha


def dossier(d, first_review, head=None):
    """One thread's evidence as text: what I asked, who answered, what moved.

    Everything here is read from the API and from git -- no model involved. That
    is the point: the expensive half of a review is deciding what the evidence
    means, and it cannot be moved off the reviewing model unless the evidence
    arrives without it.

    `head` is the MR head from `resolve_head`, never ambient HEAD: every
    path shown and every commit listed must be true at the commit the
    verdicts will be posted against.
    """
    head = head or HEAD
    n0 = d["notes"][0]
    pos = n0.get("position") or {}
    path = pos.get("new_path")
    line = pos.get("new_line") or pos.get("old_line")

    out = [f"THREAD {d['id']}", f"ANCHOR {path}:{line}", "", "--- MY NOTE ---",
           (n0.get("body") or "").strip()]

    for n in d["notes"][1:]:
        if n.get("system"):
            continue
        out += ["", f"--- REPLY by {n.get('author', {}).get('username')} "
                    f"at {(n.get('created_at') or '')[:19]} ---",
                (n.get("body") or "").strip()]
    if len(out) == 5:
        out += ["", "--- NO REPLIES: nobody wrote on this thread ---"]

    if path:
        log = git("log", "--oneline", f"--since={first_review}", head, "--", path)
        out += ["", f"--- COMMITS TOUCHING {path} SINCE REVIEW ---",
                log.strip() or "(none)"]
        if line:
            lo, hi = max(1, int(line) - CONTEXT), int(line) + CONTEXT
            lines = git("show", f"{head}:{path}").splitlines()
            out += ["", f"--- {path} @ {head} lines {lo}-{hi} ---"]
            out += [f"{i + 1:6d}| {lines[i]}"
                    for i in range(lo - 1, min(hi, len(lines)))]
    else:
        out += ["", "--- UNANCHORED: no file position on this thread ---"]
    return "\n".join(out)


def main():
    iid = sys.argv[1] if len(sys.argv) > 1 else "591"
    want = sys.argv[2] if len(sys.argv) > 2 else None

    mine, first_review = my_threads(iid)
    for d in mine:
        if want and not d["id"].startswith(want):
            continue
        print("=" * 78)
        print(dossier(d, first_review))
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
