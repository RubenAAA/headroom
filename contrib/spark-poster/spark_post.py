"""The poster. Takes a draft, replies on the threads, proves it happened.

This is the half the PoC was missing: the drafter could compose but had no
transport and no credential, so every run ended in `/tmp` as a dry run.

Three properties it has to hold, because a review poster that lacks any one of
them is worse than no poster:

  Idempotent. Every note carries a marker derived from its thread and the
  draft's content hash. A rerun re-reads the threads first and skips anything
  already carrying its marker, so a retry after a partial failure finishes the
  job instead of double-posting half of it.

  Verified. Posting is not believing. After the writes it re-lists the
  discussions from the server and confirms each marker is actually present on
  the thread it was meant for -- the check that would have caught the
  `position: null` class of silent failure, where the API returns 201 and the
  note lands somewhere useless.

  Provable. It writes a proof file naming every note id, thread and URL, plus
  the markers it verified. That file is the trust contract: the caller never
  has to take the worker's word for it, and never has to re-read the MR to
  find out what happened.

Usage:
    python3 spark_post.py <draft.json> [--dry-run]

Draft shape:
    {"iid": "591", "session_id": "...",
     "replies": [{"discussion_id": "...", "body": "..."}]}
"""

import hashlib
import json
import os
import sys
import time

import gitlab_api as gl

PROOF_DIR = os.path.expanduser("~/.local/state/spark-review")
MARKER_PREFIX = "spark-poster"


def marker_for(discussion_id, body):
    """Stable per (thread, content). Changing the text makes a new marker, so
    an edited draft posts again rather than being mistaken for already-done."""
    h = hashlib.sha256(f"{discussion_id}\x00{body}".encode()).hexdigest()[:12]
    return f"<!-- {MARKER_PREFIX}:{h} -->"


def already_posted(discussion, marker):
    return any(marker in (n.get("body") or "") for n in discussion.get("notes") or [])


def load_draft(path):
    with open(path) as fh:
        draft = json.load(fh)
    if not draft.get("replies"):
        raise SystemExit("draft has no replies")
    for r in draft["replies"]:
        if not r.get("discussion_id") or not (r.get("body") or "").strip():
            raise SystemExit(f"malformed reply entry: {r!r}")
    return draft


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    dry = "--dry-run" in sys.argv
    if not args:
        raise SystemExit(__doc__)
    draft = load_draft(args[0])
    iid = str(draft.get("iid") or "").strip()
    session = draft.get("session_id") or "nosession"
    if not iid:
        raise SystemExit("draft has no iid")

    # Server state first: what is already on these threads decides what we do.
    before = {d["id"]: d for d in gl.discussions(iid)}
    planned, skipped = [], []
    for r in draft["replies"]:
        did, body = r["discussion_id"], r["body"].rstrip()
        if did not in before:
            raise SystemExit(f"discussion {did} is not on MR !{iid}")
        mk = marker_for(did, body)
        (skipped if already_posted(before[did], mk) else planned).append(
            {"discussion_id": did, "body": body, "marker": mk,
             "resolve": bool(r.get("resolve"))}
        )

    print(f"MR !{iid}: {len(planned)} to post, {len(skipped)} already present")
    if dry:
        for p in planned:
            print(f"\n--- would reply on {p['discussion_id'][:12]} ---")
            print(p["body"])
        return 0

    posted, failed = [], []
    for p in planned:
        status, note = gl.reply(iid, p["discussion_id"], f"{p['body']}\n\n{p['marker']}")
        if status == 201 and isinstance(note, dict):
            posted.append({**p, "note_id": note.get("id")})
            print(f"posted note {note.get('id')} on {p['discussion_id'][:12]}")
        else:
            failed.append({**p, "status": status, "response": note})
            print(f"FAILED {status} on {p['discussion_id'][:12]}")
        time.sleep(0.3)

    # Re-read from the server. Nothing above is trusted for the verdict.
    after = {d["id"]: d for d in gl.discussions(iid)}
    verified, missing = [], []
    for p in posted + skipped:
        d = after.get(p["discussion_id"])
        if d and already_posted(d, p["marker"]):
            note = next(
                n for n in d["notes"] if p["marker"] in (n.get("body") or "")
            )
            verified.append({
                "discussion_id": p["discussion_id"],
                "note_id": note.get("id"),
                "marker": p["marker"],
                "author": note.get("author", {}).get("username"),
                "created_at": note.get("created_at"),
            })
        else:
            missing.append(p)

    # Resolve only what verified. A thread closed on the strength of a note
    # that never landed is the worst of both: the objection looks answered and
    # the answer is nowhere.
    verified_ids = {v["discussion_id"] for v in verified}
    resolved, resolve_failed = [], []
    for p in planned + skipped:
        if not p.get("resolve") or p["discussion_id"] not in verified_ids:
            continue
        status, _ = gl.resolve(iid, p["discussion_id"])
        if status == 200:
            resolved.append(p["discussion_id"])
            print(f"resolved {p['discussion_id'][:12]}")
        else:
            resolve_failed.append({"discussion_id": p["discussion_id"], "status": status})
            print(f"RESOLVE FAILED {status} on {p['discussion_id'][:12]}")

    os.makedirs(PROOF_DIR, exist_ok=True)
    proof_path = os.path.join(PROOF_DIR, f"{session}.proof.json")
    proof = {
        "ts": time.time(),
        "iid": iid,
        "session_id": session,
        "mr_url": f"https://rantsports.gitlab.yandexcloud.net/{gl.PROJECT}"
                  f"/-/merge_requests/{iid}",
        "planned": len(planned),
        "skipped_already_present": len(skipped),
        "posted": len(posted),
        "failed": failed,
        "verified": verified,
        "resolved": resolved,
        "resolve_failed": resolve_failed,
        "unverified": [
            {"discussion_id": m["discussion_id"], "marker": m["marker"]}
            for m in missing
        ],
        "ok": not failed and not missing and not resolve_failed,
    }
    with open(proof_path, "w") as fh:
        json.dump(proof, fh, indent=2)

    print(f"\nverified {len(verified)}/{len(posted) + len(skipped)} on the server")
    print(f"proof: {proof_path}")
    if not proof["ok"]:
        print("NOT OK: see failed/unverified in the proof")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
