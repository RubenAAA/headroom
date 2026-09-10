"""Build the draft from the MR itself, not from the reviewing model's leftovers.

The version this replaces read the reviewing model's transcript tail and asked
a worker to reshape it into comments. That cannot save anything, because it
needs the verdicts to already exist before it runs -- it transcribes work
instead of taking it over, and the whole cost of a review is in the work.

So the evidence comes from primary sources here. `thread_dossier` pulls each
thread and the git history behind its anchor; worker calls turn that into
verdicts. The reviewing model contributes the MR number.

One call per GROUP of threads, not per thread: fourteen sequential calls at
~65s each overran the supervisor timeout one thread short (MR !597), because
the per-call overhead -- spawn, queue, stream -- dominates the thinking.
A group of six dossiers is still a small context, and anything the batch
call drops or mangles falls back to one single-thread call, so a worker
that fails on thread 7 costs thread 7 one extra call rather than the batch.
Group size comes from SPARK_DRAFT_BATCH (default 6).

Output is the poster's schema, so `spark-goahead.sh` can take it unchanged:

    {"iid": "591", "session_id": "...",
     "replies": [{"discussion_id": "...", "body": "...", "resolve": false}]}

Usage:
    SPARK_REVIEW_WORKER=1 python3 spark_draft.py <iid> [session_id] [mode] [transcript]

Mode is the review command that armed the session: `gitlab-review`
(default) drafts follow-ups on threads I opened, or -- when I have opened
none yet -- new threads from the review findings (first-round review);
`fix-mr-comments` drafts `fix-mr-comments` drafts
answers to reviewers' still-open threads on my own MR.
"""

import json
import os
import re
import subprocess
import sys

import thread_dossier as td

OUTDIR = os.path.expanduser("~/.local/state/spark-review")
MODEL = os.environ.get("SPARK_DRAFT_MODEL", "claude-muse-spark-1.2")
PROXY = os.environ.get("SPARK_DRAFT_BASE_URL", "http://127.0.0.1:8787")
TIMEOUT = int(os.environ.get("SPARK_DRAFT_TIMEOUT", "180"))
BATCH = int(os.environ.get("SPARK_DRAFT_BATCH", "6"))
BATCH_TIMEOUT = int(os.environ.get("SPARK_DRAFT_BATCH_TIMEOUT", "600"))

TASK = """You are answering ONE review thread you opened on a merge request.

Below is everything known about it: the note you wrote, any replies, the
commits that touched the anchored file since the review, and that file's
current contents around the anchor.

Decide whether your original objection has been addressed by the code as it
now stands. Replies may be absent -- a fix can land as a commit and nothing
else, and silence is not the same as "unaddressed".

Write the reply you would post on the thread. Say, in this order: what
actually changed, whether that settles the objection, and what follows. If
you are closing, the reason for closing. If you are not, what is still
missing and what would close it. Cite files and lines you can see in the
evidence. Never claim a change you cannot point to.

Answer in the language the original note is written in.

Output ONLY a JSON object, no prose around it:
{"resolve": true or false, "body": "the reply text"}

resolve is true only if the objection is fully addressed.

EVIDENCE:
"""

FIX_TASK = """You authored this merge request, and a reviewer left the thread below.

Below is everything known about it: the reviewer's note, any replies so far,
the commits that touched the anchored file since the review, and that file's
current contents around the anchor.

Decide what the code as it now stands says about the reviewer's point, then
write the reply you would post on the thread. Say, in this order: what
actually changed (or what you checked), whether that settles the point, and
what follows. If you are closing, the reason for closing. If you are not,
what is still missing and what would close it. Cite files and lines you can
see in the evidence. Never claim a change you cannot point to. If the
reviewer is right and nothing has changed yet, say so plainly and say what
you will do -- do not argue the thread closed.

Answer in the language the reviewer's note is written in.

Output ONLY a JSON object, no prose around it:
{"resolve": true or false, "body": "the reply text"}

resolve is true only if the reviewer's point is fully addressed.

EVIDENCE:
"""


NEW_THREADS_TASK = """You are posting a first-round code review as new threads
on a merge request. There are no existing threads: everything below opens one.

The review findings follow at the end (the approved table or text near the end
matters most; the rest is tool noise -- ignore it). Turn each finding into one
thread entry. Output ONLY a JSON object, no prose around it:
{"threads": [{"key": "finding-1", "kind": "inline", "file": "path/from/repo/root",
"line": 123, "side": "new", "body": "the review comment"}, ...]}

Rules, in order of importance:

- key is the finding number: finding-1, finding-2, and so on. It is the
  idempotency identity across retries, so keep it stable and unique.
- kind inline ONLY when the finding cites an exact file and line in the MR
  diff. Anything else -- a docs item, a runbook note, a line outside the
  diff -- is kind discussion, with no file or line keys at all.
- side is "new" unless the finding is about removed code, then "old".
- file is the repo-root-relative path exactly as cited. Never invent a line
  number: no line, no inline.
- body is the review comment, self-contained: what is wrong, where, and what
  should change. Never paste redaction placeholder tokens as values; a value
  that arrives masked stays out or is marked [redacted].

FINDINGS:
"""


def _call(prompt, timeout):
    """Run the worker once. Returns (stdout, diagnostic).

    The diagnostic is what makes NO VERDICT lines actionable: a nonzero rc
    with empty stdout looks exactly like a bad answer unless it is named.
    """
    env = {**os.environ,
           "SPARK_REVIEW_WORKER": "1",
           "ANTHROPIC_BASE_URL": PROXY}
    try:
        r = subprocess.run(
            ["claude", "-p", "--model", MODEL, prompt],
            capture_output=True, text=True, timeout=timeout, env=env,
        )
    except subprocess.TimeoutExpired:
        return None, "worker call timed out"
    if r.returncode != 0 and not r.stdout.strip():
        tail = (r.stderr or "").strip().splitlines()[-3:]
        return None, f"worker rc={r.returncode} empty stdout" + (
            f" stderr: {' | '.join(tail)}" if tail else "")
    return r.stdout, None


def ask(prompt):
    out, diag = _call(prompt, TIMEOUT)
    if out is None:
        return None, diag
    verdict = parse(out)
    return verdict, (None if verdict else diag or "unparseable worker output")


def parse(out):
    """Take the verdict out of whatever the worker wrapped it in.

    Models fence JSON, prepend a sentence, or both. Failing a thread because
    its answer arrived inside a code fence would leave the thread silent, which
    is the outcome this exists to prevent -- so try the whole string, then the
    fence, then the outermost braces.
    """
    for cand in (out,
                 *re.findall(r"```(?:json)?\s*(.*?)```", out, re.S),
                 *re.findall(r"(\{.*\})", out, re.S)):
        try:
            v = json.loads(cand.strip())
        except (json.JSONDecodeError, AttributeError):
            continue
        if isinstance(v, dict) and (v.get("body") or "").strip():
            return {"body": v["body"].strip(), "resolve": bool(v.get("resolve"))}
    return None


def batch_prompt(task, items):
    """One prompt for a group of threads. The task's per-thread instructions
    stand unchanged; this only frames them as independent items sharing one
    call, each answered in the language of its own opening note.
    """
    parts = [task.rstrip() + "\n\nYou are answering SEVERAL threads, not one. "
             "Decide each one independently from its own evidence -- never "
             "carry a conclusion from one thread into another, and never "
             "mention the other threads in a reply.\n\n"
             "Output ONLY a JSON array, no prose around it, one object per "
             "thread in the order given:\n"
             '[{"discussion_id": "<the thread id>", '
             '"resolve": true or false, "body": "<the reply text>"}]']
    for did, evidence in items:
        parts.append(f"\n===== THREAD {did} =====\n{evidence}")
    return "\n".join(parts)


def parse_batch(out, ids):
    """Take per-thread verdicts out of a batch answer. Returns {did: verdict}
    for the items that parsed; anything missing or mangled is simply absent
    and the caller retries those threads one by one. Strict here is cheap
    because the fallback is the old reliable path, not silence.
    """
    found = {}
    if not out:
        return found
    cands = [out, *re.findall(r"```(?:json)?\s*(.*?)```", out, re.S)]
    # A bare array with trailing prose defeats json.loads; the outermost
    # brackets are worth one try each.
    cands += re.findall(r"(\[.*\])", out, re.S)
    for cand in cands:
        try:
            v = json.loads(cand.strip())
        except (json.JSONDecodeError, AttributeError):
            continue
        items = v if isinstance(v, list) else v.get("replies")
        if not isinstance(items, list):
            continue
        for it in items:
            if not isinstance(it, dict):
                continue
            did = it.get("discussion_id")
            if did in ids and did not in found and (it.get("body") or "").strip():
                found[did] = {"body": it["body"].strip(),
                              "resolve": bool(it.get("resolve"))}
        if found:
            return found
    return found


def parse_threads(out):
    """Take new-thread entries out of a first-round answer. Returns the list;
    empty when nothing parsed. Strict like parse_batch: a mangled answer is a
    loud failure downstream, not a partial post."""
    found = []
    if not out:
        return found
    cands = [out, *re.findall(r"```(?:json)?\s*(.*?)```", out, re.S)]
    cands += re.findall(r"(\[.*\])", out, re.S)
    for cand in cands:
        try:
            v = json.loads(cand.strip())
        except (json.JSONDecodeError, AttributeError):
            continue
        items = v if isinstance(v, list) else v.get("threads")
        if not isinstance(items, list):
            continue
        for it in items:
            if not isinstance(it, dict) or not (it.get("body") or "").strip():
                continue
            if it.get("kind") not in (None, "inline", "discussion"):
                continue
            if it.get("kind") == "inline" and (it.get("file") is None
                                               or it.get("line") is None):
                continue
            entry = {"body": it["body"].strip()}
            for k in ("key", "kind", "file", "line", "side"):
                if it.get(k) is not None:
                    entry[k] = it[k]
            found.append(entry)
        if found:
            return found
    return found


def read_findings(transcript, limit=60000):
    """The review findings live near the end of the arming session's
    transcript (the approved table or text); tool noise fills the rest, so a
    bounded tail is the evidence. Returns None when unreadable."""
    try:
        with open(transcript, errors="replace") as fh:
            fh.seek(0, os.SEEK_END)
            size = fh.tell()
            fh.seek(max(0, size - limit))
            return fh.read()
    except OSError:
        return None


def first_round(iid, session, transcript):
    """gitlab-review with no threads of mine: open new ones from findings."""
    if not transcript:
        print("first-round review needs the session transcript: "
              "spark_draft.py <iid> <session> gitlab-review <transcript>",
              file=sys.stderr)
        return 1
    head = td.resolve_head(iid)
    print(f"  evidence at MR head {head[:12]}", file=sys.stderr)
    findings = read_findings(transcript)
    if not findings or not findings.strip():
        print("empty transcript tail; nothing to draft from", file=sys.stderr)
        return 1
    threads = []
    for attempt in (1, 2):
        out, diag = _call(NEW_THREADS_TASK + findings, TIMEOUT)
        threads = parse_threads(out) if out is not None else []
        if threads:
            break
        print(f"  threads draft attempt {attempt} failed "
              f"({diag or 'unparseable worker output'})", file=sys.stderr)
    if not threads:
        print("no threads drafted; nothing to post", file=sys.stderr)
        return 1
    os.makedirs(OUTDIR, exist_ok=True)
    path = os.path.join(OUTDIR, f"{session}.draft.json")
    with open(path, "w") as fh:
        json.dump({"iid": iid, "session_id": session, "threads": threads,
                   "head_sha": head, "partial": False},
                  fh, ensure_ascii=False, indent=2)
    print(f"draft: {path} ({len(threads)} new threads)", file=sys.stderr)
    print(path)
    return 0


def main():
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    iid = sys.argv[1]
    session = sys.argv[2] if len(sys.argv) > 2 else f"mr{iid}"
    mode = sys.argv[3] if len(sys.argv) > 3 else "gitlab-review"
    transcript = sys.argv[4] if len(sys.argv) > 4 else None
    if mode not in ("gitlab-review", "fix-mr-comments"):
        raise SystemExit(f"unknown mode {mode!r}: want gitlab-review or fix-mr-comments")
    task = FIX_TASK if mode == "fix-mr-comments" else TASK

    mine, first_review = td.my_threads(iid, mode)
    if not mine:
        if mode == "gitlab-review":
            # First-round review: no threads of mine exist because none have
            # been opened yet. Draft new threads from the review findings
            # instead of verdicts on threads.
            raise SystemExit(first_round(iid, session, transcript))
        raise SystemExit(f"MR !{iid}: no open reviewer threads")
    print(f"MR !{iid} [{mode}]: {len(mine)} threads in scope", file=sys.stderr)
    head = td.resolve_head(iid)
    print(f"  evidence at MR head {head[:12]}", file=sys.stderr)

    os.makedirs(OUTDIR, exist_ok=True)
    path = os.path.join(OUTDIR, f"{session}.draft.json")

    def flush():
        """Write what we have. The loop below can die on any thread --
        per-thread timeout, supervisor timeout, SIGKILL -- and a draft
        written only at the end turns every finished verdict into waste
        (MR !597: 13 of 14 verdicts discarded one thread short of done).
        A partial draft posts what exists and names what it does not cover;
        the poster only ever sends listed replies, so extra keys are safe.
        """
        if not replies:
            return
        with open(path, "w") as fh:
            json.dump({"iid": iid, "session_id": session, "replies": replies,
                       "threads": threads,
                       "head_sha": head,
                       "partial": bool(failed) or len(replies) < len(mine),
                       "unverdict_threads": [did[:12] for did in failed]},
                      fh, ensure_ascii=False, indent=2)

    replies, failed, threads = [], [], []
    groups = [mine[i:i + BATCH] for i in range(0, len(mine), BATCH)]
    print(f"  {len(mine)} threads in {len(groups)} batch call(s)", file=sys.stderr)
    for group in groups:
        items = [(d["id"], td.dossier(d, first_review, head)) for d in group]
        ids = [did for did, _ in items]
        out, diag = _call(batch_prompt(task, items), BATCH_TIMEOUT)
        batch = parse_batch(out, ids) if out is not None else {}
        if out is None:
            print(f"  batch call failed ({diag}); retrying {len(ids)} thread(s) singly",
                  file=sys.stderr)
        for did, evidence in items:
            if did in batch:
                verdict = batch[did]
                replies.append({"discussion_id": did, **verdict})
                print(f"  {did[:12]} {'close' if verdict['resolve'] else 'keep '}",
                      file=sys.stderr)
                continue
            verdict, diag = ask(task + evidence)
            if verdict:
                replies.append({"discussion_id": did, **verdict})
                print(f"  {did[:12]} {'close' if verdict['resolve'] else 'keep '} (single retry)",
                      file=sys.stderr)
            else:
                failed.append(did)
                print(f"  {did[:12]} NO VERDICT ({diag})", file=sys.stderr)
        flush()

    if not replies and not threads:
        raise SystemExit("no verdicts produced; nothing to draft")

    # Threads with no verdict are named, not swallowed. A draft that silently
    # covered 9 of 11 would read as complete and leave two objections
    # unanswered with nothing saying which.
    if failed:
        print(f"WARNING: no verdict for {len(failed)}: "
              f"{', '.join(f[:12] for f in failed)}", file=sys.stderr)
    print(f"draft: {path} ({len(replies)} replies, "
          f"{sum(1 for r in replies if r['resolve'])} to close)", file=sys.stderr)
    print(path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
