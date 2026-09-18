"""One dossier per review thread: what I asked, and what the code says now.

Pairs each thread's opening note with the current state of the lines it was
anchored to, plus whether that file moved in the commits that postdate the
review. That is the whole evidence base for "was this addressed" -- the
alternative, judging from the thread alone, cannot see a fix that landed as a
commit rather than a reply.

Anchor-only evidence is not enough on its own: fixes land in files the
anchor never named. Every dossier therefore also carries the branch-wide
commit list since the review plus the diff of the files the thread itself
names, so a fix outside the anchored file still shows up.
"""

import os
import re
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
# Bounds for the branch-wide evidence below. Each dossier is copied into a
# batched worker prompt, so these multiply by batch size: keep them tight.
BRANCH_LOG_LIMIT = 30
MENTIONED_FILE_LIMIT = 5
MENTIONED_DIFF_MAX_LINES = 80


def git(*args):
    r = subprocess.run(["git", "-C", REPO, *args], capture_output=True, text=True)
    return r.stdout


def mentioned_paths(text):
    """Repo-relative file paths named in the thread text.

    Reviewers cite the files a fix must touch (`internal/metrics/...`);
    the anchor only names where the note sits. A fix that lands in a cited
    file but not the anchored one is invisible to anchor-only evidence, so
    these paths get their own commit list + diff below.
    """
    paths = []
    for m in re.finditer(
        r'(?:^|[\s`"\'(])((?:internal|scripts|docs|crates|contrib)/'
        r'[A-Za-z0-9_.\-/]+\.[A-Za-z0-9]+)', text):
        p = m.group(1).rstrip('.,:;)')
        if p and p not in paths:
            paths.append(p)
        if len(paths) >= MENTIONED_FILE_LIMIT:
            break
    return paths


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

    A thread I already answered stays out: anything I wrote after the
    reviewer's last note is my reply, and drafting a second one next to it
    (MR !597: 11 replies, several doubling answers already on the threads)
    reads as arguing with myself. System notes are not replies -- resolves
    and moves land as system notes and must not count as answered.
    """
    ds = [d for d in discussions
          if not d.get("individual_note") and d.get("notes")]
    if mode == "fix-mr-comments":
        return [d for d in ds
                if not d.get("resolved")
                and d["notes"][0].get("author", {}).get("username") != me
                and not _already_answered(d, me)]
    return [d for d in ds
            if d["notes"][0].get("author", {}).get("username") == me]


def _already_answered(discussion, me):
    """True when I wrote on the thread after the reviewer's last note."""
    notes = [n for n in (discussion.get("notes") or []) if not n.get("system")]
    if not notes:
        return False
    last_reviewer = -1
    for i, n in enumerate(notes):
        if n.get("author", {}).get("username") != me:
            last_reviewer = i
    if last_reviewer < 0:
        return True
    return any(n.get("author", {}).get("username") == me
               for n in notes[last_reviewer + 1:])


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
        if log.strip():
            # The commit titles above claim the fix; the diff proves it --
            # and proves it landed in the hunk this thread cares about
            # rather than somewhere else in the file.
            adiff = git("log", f"--since={first_review}", "-p",
                        "--format=COMMIT %h %s", "-U3", head, "--", path)
            alines = adiff.splitlines()
            if len(alines) > MENTIONED_DIFF_MAX_LINES:
                alines = (alines[:MENTIONED_DIFF_MAX_LINES]
                          + ["... (truncated)"])
            out += [f"--- DIFF {path} SINCE REVIEW ---"] + (alines or ["(empty)"])
        # FINDING-047: GitLab line fields are ints when present, but a
        # defensive int() broke the whole batch on one malformed thread.
        # Non-numeric anchors keep the commits list and skip the snippet.
        try:
            lineno = int(line) if line else 0
        except (TypeError, ValueError):
            lineno = 0
        if lineno:
            lo, hi = max(1, lineno - CONTEXT), lineno + CONTEXT
            lines = git("show", f"{head}:{path}").splitlines()
            out += ["", f"--- {path} @ {head} lines {lo}-{hi} "
                         f"(anchor line is from review time; code may have moved) ---"]
            out += [f"{i + 1:6d}| {lines[i]}"
                    for i in range(lo - 1, min(hi, len(lines)))]
    else:
        out += ["", "--- UNANCHORED: no file position on this thread ---"]

    # Branch-wide evidence. The fix for this thread can land in a file the
    # anchor never named (MR !612: the ~0.4% figures were fixed in
    # internal/metrics/ + monitoring/ while the thread sits anchored on
    # store/data_invariants.go). Anchor-only evidence reads that as "nothing
    # changed" at every head, forever -- so every dossier also carries what
    # the branch did since the review, and what changed in the files the
    # thread itself names.
    blog = git("log", "--oneline", f"--since={first_review}",
               f"--max-count={BRANCH_LOG_LIMIT}", head, "--")
    out += ["", "--- ALL BRANCH COMMITS SINCE REVIEW ---",
            blog.strip() or "(none)"]
    # Only the thread's own words name the files a fix may live in -- the
    # snippet above is file contents and would match every path it mentions.
    thread_text = (n0.get("body") or "") + "\n" + "\n".join(
        (n.get("body") or "") for n in d["notes"][1:] if not n.get("system"))
    for mp in mentioned_paths(thread_text):
        if mp == path:
            continue
        mlog = git("log", "--oneline", f"--since={first_review}", head,
                   "--", mp)
        out += ["", f"--- COMMITS TOUCHING MENTIONED FILE {mp} SINCE REVIEW ---",
                mlog.strip() or "(none)"]
        if mlog.strip():
            diff = git("log", f"--since={first_review}", "-p",
                       "--format=COMMIT %h %s", "-U3", head, "--", mp)
            dlines = diff.splitlines()
            if len(dlines) > MENTIONED_DIFF_MAX_LINES:
                dlines = (dlines[:MENTIONED_DIFF_MAX_LINES]
                          + ["... (truncated)"])
            out += [f"--- DIFF {mp} SINCE REVIEW ---"] + (dlines or ["(empty)"])
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
