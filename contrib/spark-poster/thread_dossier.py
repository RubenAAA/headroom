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
REPO = os.environ.get("SPARK_REPO", (
    "/home/ruben/meta/ai-first-workspace/internal-b2b/"
    "b2b-technology/platform/b2b-amg"))
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
    ds = [d for d in gl.discussions(iid) if not d.get("individual_note")]
    mine = select_threads(ds, ME, mode)
    if not mine:
        return [], None
    return mine, min(d["notes"][0]["created_at"] for d in mine)


def dossier(d, first_review):
    """One thread's evidence as text: what I asked, who answered, what moved.

    Everything here is read from the API and from git -- no model involved. That
    is the point: the expensive half of a review is deciding what the evidence
    means, and it cannot be moved off the reviewing model unless the evidence
    arrives without it.
    """
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
        log = git("log", "--oneline", f"--since={first_review}", HEAD, "--", path)
        out += ["", f"--- COMMITS TOUCHING {path} SINCE REVIEW ---",
                log.strip() or "(none)"]
        if line:
            lo, hi = max(1, int(line) - CONTEXT), int(line) + CONTEXT
            lines = git("show", f"{HEAD}:{path}").splitlines()
            out += ["", f"--- {path} @ {HEAD} lines {lo}-{hi} ---"]
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
