"""The poster. Takes a draft, replies on threads and opens new ones, proves it happened.

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
     "replies": [{"discussion_id": "...", "body": "...", "resolve": false}],
     "threads": [{"key": "finding-1", "kind": "inline", "file": "src/x.py",
                  "line": 123, "side": "new", "body": "..."},
                 {"key": "finding-3", "kind": "discussion", "body": "..."}]}

Replies answer existing threads (the fix-mr-comments shape). Threads open
new ones (the first-round gitlab-review shape): kind inline anchors on
file:line of the live diff, kind discussion is MR-level. Both halves share
the marker, verification and proof machinery below.
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


def marker_for_new(identity, body):
    """Stable per (new-thread identity, content), mirroring marker_for.

    Identity is the draft's key when given, else kind:file:line for inline
    threads. Plain discussions have no anchor, so distinct findings need
    distinct keys or distinct bodies -- identical bodies dedupe, which is the
    idempotent behavior a retry wants anyway."""
    h = hashlib.sha256(f"{identity}\x00{body}".encode()).hexdigest()[:12]
    return f"<!-- {MARKER_PREFIX}:{h} -->"


def find_marker(discussions, marker):
    """(discussion_id, note) of the first note carrying marker, else None."""
    for did, d in discussions.items():
        for n in d.get("notes") or []:
            if marker in (n.get("body") or ""):
                return did, n
    return None


def thread_kind(t):
    """Explicit kind wins; otherwise file+line means inline, else discussion."""
    kind = t.get("kind") or ("inline" if t.get("file") is not None else "discussion")
    if kind not in ("inline", "discussion"):
        raise SystemExit(f"malformed thread entry (bad kind): {t!r}")
    if kind == "inline" and (t.get("file") is None or t.get("line") is None):
        raise SystemExit(f"inline thread needs file and line: {t!r}")
    return kind


def thread_identity(t, index):
    if t.get("key"):
        return str(t["key"])
    if t.get("file") is not None:
        return f"inline:{t['file']}:{t['line']}"
    return f"discussion:{index}"


def inline_position(refs, t):
    """Anchor a draft inline thread to the live MR diff. SHAs come from the
    MR at post time, never the draft; file, line and side come from the draft."""
    pos = {"base_sha": refs["base_sha"], "head_sha": refs["head_sha"],
           "start_sha": refs["start_sha"], "position_type": "text"}
    line = int(t["line"])
    if str(t.get("side", "new")) == "old":
        pos["old_path"] = t["file"]
        pos["old_line"] = line
    else:
        pos["new_path"] = t["file"]
        pos["new_line"] = line
    return pos


def load_draft(path):
    with open(path) as fh:
        draft = json.load(fh)
    draft["replies"] = draft.get("replies") or []
    draft["threads"] = draft.get("threads") or []
    if not draft["replies"] and not draft["threads"]:
        raise SystemExit("draft has no replies and no new threads")
    for r in draft["replies"]:
        if not r.get("discussion_id") or not (r.get("body") or "").strip():
            raise SystemExit(f"malformed reply entry: {r!r}")
    for t in draft["threads"]:
        if not (t.get("body") or "").strip():
            raise SystemExit(f"malformed thread entry (no body): {t!r}")
        thread_kind(t)
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

    # The draft is evidence against one commit. If the MR moved since the
    # draft was written, the verdicts describe code that is no longer there
    # -- posting them repeats MR !597, where a fix pushed 20 minutes before
    # the run drew 14 "nothing changed" replies and two wrong resolves.
    # Refuse loudly instead; the operator re-runs and the new draft sees
    # the new head. Drafts predating head_sha skip the check.
    # An unreadable head refuses too: an API error must never read as
    # "unchanged" (fail-open posted stale verdicts against an unknown head).
    def refuse(reason):
        os.makedirs(PROOF_DIR, exist_ok=True)
        proof_path = os.path.join(PROOF_DIR, f"{session}.proof.json")
        with open(proof_path, "w") as fh:
            json.dump({"ts": time.time(), "iid": iid, "session_id": session,
                       "mr_url": None, "planned": 0,
                       "skipped_already_present": 0, "posted": 0,
                       "failed": [], "verified": [], "resolved": [],
                       "resolve_failed": [], "unverified": [],
                       "ok": False, "refused": reason}, fh, indent=2)
        print(f"REFUSED: {reason}")
        print(f"proof: {proof_path}")
        return 1

    if draft.get("head_sha"):
        try:
            mr = gl.merge_request(iid) or {}
        except Exception as e:
            return refuse(f"could not read MR !{iid} head from GitLab ({e}); "
                          f"not posting against an unknown commit")
        now = mr.get("sha") or ""
        if not now:
            return refuse(f"could not read MR !{iid} head from GitLab; "
                          f"not posting against an unknown commit")
        if now != draft["head_sha"]:
            return refuse(f"MR !{iid} moved since the draft: drafted at "
                          f"{draft['head_sha'][:12]}, head is now {now[:12]}; "
                          f"not posting")

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

    # New threads have no discussion yet, so each marker is searched across
    # every discussion on the MR -- a retry after a partial failure finds
    # what the first run already opened instead of opening it twice.
    planned_new, skipped_new = [], []
    refs = None
    for i, t in enumerate(draft["threads"]):
        body = t["body"].rstrip()
        kind = thread_kind(t)
        ident = thread_identity(t, i)
        mk = marker_for_new(ident, body)
        entry = {"identity": ident, "body": body, "marker": mk, "kind": kind}
        if kind == "inline":
            entry["file"], entry["line"] = t["file"], t["line"]
            entry["side"] = str(t.get("side", "new"))
            if refs is None:
                refs = gl.diff_refs(iid)
                if refs is None:
                    raise SystemExit(f"MR !{iid}: no diff_refs for inline threads")
        if find_marker(before, mk):
            skipped_new.append(entry)
        else:
            planned_new.append(entry)

    print(f"MR !{iid}: {len(planned)} replies + {len(planned_new)} new threads "
          f"to post, {len(skipped) + len(skipped_new)} already present")
    if dry:
        for p in planned:
            print(f"\n--- would reply on {p['discussion_id'][:12]} ---")
            print(p["body"])
        for p in planned_new:
            where = (f"{p['file']}:{p['line']}" if p["kind"] == "inline"
                     else "MR-level discussion")
            print(f"\n--- would open {p['kind']} thread ({where}) ---")
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

    created, create_failed = [], []
    for p in planned_new:
        if p["kind"] == "inline":
            status, disc = gl.create_discussion(
                iid, f"{p['body']}\n\n{p['marker']}",
                inline_position(refs, p))
        else:
            status, disc = gl.create_discussion(
                iid, f"{p['body']}\n\n{p['marker']}")
        if status == 201 and isinstance(disc, dict):
            created.append({**p, "discussion_id": disc.get("id")})
            print(f"opened {p['kind']} thread {disc.get('id')}")
        else:
            create_failed.append({**p, "status": status, "response": disc})
            print(f"CREATE FAILED {status} ({p['kind']} {p['identity']})")
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

    # New threads verify by marker-anywhere: the discussion did not exist
    # at plan time, so there is no thread to check -- only the marker the
    # fresh discussion must carry.
    created_ok, created_missing = [], []
    for p in created + skipped_new:
        hit = find_marker(after, p["marker"])
        if hit:
            did, note = hit
            created_ok.append({
                "discussion_id": did,
                "note_id": note.get("id"),
                "marker": p["marker"],
                "author": note.get("author", {}).get("username"),
                "created_at": note.get("created_at"),
            })
        else:
            created_missing.append(p)

    # Resolve only what verified. A thread closed on the strength of a note
    # that never landed is the worst of both: the objection looks answered and
    # the answer is nowhere.
    # Each resolve is confirmed against a FRESH single-thread read, not the
    # batch verdict: the drafter may have mixed up threads, the MR may have
    # moved under it, or a human may have objected since. Resolve fires only
    # when the thread is still open, our marker note is on it, and nobody
    # spoke after us. Anything else lands in resolve_failed with a reason
    # instead of closing someone's thread.
    verified_ids = {v["discussion_id"] for v in verified}
    resolved, resolve_failed = [], []

    def confirm_for_resolve(did, marker):
        """Fresh single-thread read; None when the close is safe, else why not."""
        disc = gl.discussion(iid, did)
        if not disc:
            return "thread unreadable on confirm"
        notes = [n for n in (disc.get("notes") or []) if not n.get("system")]
        mine = [n for n in notes if marker in (n.get("body") or "")]
        if not mine:
            return "our note no longer on thread"
        if disc.get("resolved", False):
            return "already-resolved"
        last = notes[-1] if notes else {}
        if marker not in (last.get("body") or ""):
            return "new activity after our note"
        return None

    for p in planned + skipped:
        if not p.get("resolve") or p["discussion_id"] not in verified_ids:
            continue
        did = p["discussion_id"]
        reason = confirm_for_resolve(did, p["marker"])
        if reason == "already-resolved":
            resolved.append(did)
            print(f"already resolved {did[:12]}")
            continue
        if reason is not None:
            resolve_failed.append({"discussion_id": did, "reason": reason})
            print(f"RESOLVE REFUSED ({reason}) on {did[:12]}")
            continue
        status, _ = gl.resolve(iid, did)
        if status == 200:
            resolved.append(did)
            print(f"resolved {did[:12]}")
        else:
            resolve_failed.append({"discussion_id": did, "status": status})
            print(f"RESOLVE FAILED {status} on {did[:12]}")

    os.makedirs(PROOF_DIR, exist_ok=True)
    proof_path = os.path.join(PROOF_DIR, f"{session}.proof.json")
    web_base, project = gl.project()
    proof = {
        "ts": time.time(),
        "iid": iid,
        "session_id": session,
        "mr_url": f"{web_base}/{project}/-/merge_requests/{iid}",
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
        "threads_planned": len(planned_new),
        "threads_skipped_present": len(skipped_new),
        "threads_created": len(created),
        "created": created_ok,
        "create_failed": create_failed,
        "threads_unverified": [
            {"identity": m["identity"], "marker": m["marker"]}
            for m in created_missing
        ],
        "ok": not failed and not missing and not resolve_failed
        and not create_failed and not created_missing,
    }
    with open(proof_path, "w") as fh:
        json.dump(proof, fh, indent=2)

    print(f"\nverified {len(verified)}/{len(posted) + len(skipped)} replies, "
          f"{len(created_ok)}/{len(created) + len(skipped_new)} new threads on the server")
    print(f"proof: {proof_path}")
    if not proof["ok"]:
        print("NOT OK: see failed/unverified/resolve_failed in the proof")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
