"""Build the draft from the MR itself, not from the reviewing model's leftovers.

The version this replaces read the reviewing model's transcript tail and asked
a worker to reshape it into comments. That cannot save anything, because it
needs the verdicts to already exist before it runs -- it transcribes work
instead of taking it over, and the whole cost of a review is in the work.

So the evidence comes from primary sources here. `thread_dossier` pulls each
thread and the git history behind its anchor; one worker call per thread turns
that into a verdict. The reviewing model contributes the MR number.

One call per thread rather than one for all of them: a thread's dossier plus
its verdict is a small context, eleven of them concatenated is not, and a
worker that fails on thread 7 should cost thread 7 rather than the batch.

Output is the poster's schema, so `spark-goahead.sh` can take it unchanged:

    {"iid": "591", "session_id": "...",
     "replies": [{"discussion_id": "...", "body": "...", "resolve": false}]}

Usage:
    SPARK_REVIEW_WORKER=1 python3 spark_draft.py <iid> [session_id]
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


def ask(prompt):
    env = {**os.environ,
           "SPARK_REVIEW_WORKER": "1",
           "ANTHROPIC_BASE_URL": PROXY}
    r = subprocess.run(
        ["claude", "-p", "--model", MODEL, prompt],
        capture_output=True, text=True, timeout=TIMEOUT, env=env,
    )
    return r.stdout


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


def main():
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    iid = sys.argv[1]
    session = sys.argv[2] if len(sys.argv) > 2 else f"mr{iid}"

    mine, first_review = td.my_threads(iid)
    if not mine:
        raise SystemExit(f"MR !{iid}: no threads opened by {td.ME}")
    print(f"MR !{iid}: {len(mine)} threads by {td.ME}", file=sys.stderr)

    replies, failed = [], []
    for d in mine:
        did = d["id"]
        try:
            verdict = parse(ask(TASK + td.dossier(d, first_review)))
        except subprocess.TimeoutExpired:
            verdict = None
        if verdict:
            replies.append({"discussion_id": did, **verdict})
            print(f"  {did[:12]} {'close' if verdict['resolve'] else 'keep '}",
                  file=sys.stderr)
        else:
            failed.append(did)
            print(f"  {did[:12]} NO VERDICT", file=sys.stderr)

    if not replies:
        raise SystemExit("no verdicts produced; nothing to draft")

    os.makedirs(OUTDIR, exist_ok=True)
    path = os.path.join(OUTDIR, f"{session}.draft.json")
    with open(path, "w") as fh:
        json.dump({"iid": iid, "session_id": session, "replies": replies},
                  fh, ensure_ascii=False, indent=2)

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
